//! Defines an interface for register-like actors (via [`RegisterMsg`]) and also provides
//! [`RegisterTestSystem`] for model checking.

use crate::Property;
use crate::actor::{Actor, Id, Out};
use crate::actor::system::{DuplicatingNetwork, LossyNetwork, System, SystemModel, SystemState};
use crate::semantics::register::{Register, RegisterOp, RegisterRet};
use crate::semantics::LinearizabilityTester;
use std::borrow::Cow;
use std::fmt::Debug;
use std::hash::Hash;

/// Defines an interface for a register-like actor.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[derive(serde::Serialize, serde::Deserialize)]
pub enum RegisterMsg<RequestId, Value, InternalMsg> {
    /// A message specific to the register system's internal protocol.
    Internal(InternalMsg),

    /// Indicates that a value should be written.
    Put(RequestId, Value),
    /// Indicates that a value should be retrieved.
    Get(RequestId),

    /// Indicates a successful `Put`. Analogous to an HTTP 2XX.
    PutOk(RequestId),
    /// Indicates a successful `Get`. Analogous to an HTTP 2XX.
    GetOk(RequestId, Value),
}
use RegisterMsg::*;

/// A system for testing an actor service with register semantics.
#[derive(Clone)]
pub struct RegisterTestSystem<ServerActor, InternalMsg>
where
    ServerActor: Actor<Msg = RegisterMsg<TestRequestId, TestValue, InternalMsg>> + Clone,
    InternalMsg: Clone + Debug + Eq + Hash,
{
    pub servers: Vec<ServerActor>,
    pub client_count: u8,
    pub within_boundary: fn(state: &SystemState<Self>) -> bool,
    pub lossy_network: LossyNetwork,
    pub duplicating_network: DuplicatingNetwork,
}

impl<ServerActor, InternalMsg> Default for RegisterTestSystem<ServerActor, InternalMsg>
    where
    ServerActor: Actor<Msg = RegisterMsg<TestRequestId, TestValue, InternalMsg>> + Clone,
    InternalMsg: Clone + Debug + Eq + Hash,
{
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            client_count: 2,
            within_boundary: |_| true,
            lossy_network: LossyNetwork::No,
            duplicating_network: DuplicatingNetwork::Yes,
        }
    }
}

impl<ServerActor, InternalMsg> System for RegisterTestSystem<ServerActor, InternalMsg>
    where
        ServerActor: Actor<Msg = RegisterMsg<TestRequestId, TestValue, InternalMsg>> + Clone,
        InternalMsg: Clone + Debug + Eq + Hash,
{
    type Actor = RegisterActor<ServerActor>;
    type History = LinearizabilityTester<Id, Register<TestValue>>;

    fn actors(&self) -> Vec<Self::Actor> {
        let mut actors: Vec<Self::Actor> = self.servers.iter().map(|s| {
            RegisterActor::Server(s.clone())
        }).collect();
        for _ in 0..self.client_count {
            actors.push(RegisterActor::Client { server_count: self.servers.len() as u64 });
        }
        actors
    }

    fn lossy_network(&self) -> LossyNetwork {
        self.lossy_network
    }

    fn duplicating_network(&self) -> DuplicatingNetwork {
        self.duplicating_network
    }

    fn record_msg_out(&self, history: &Self::History, src: Id, _dst: Id, msg: &<Self::Actor as Actor>::Msg) -> Option<Self::History> {
        // FIXME: Currently throws away useful information about invalid histories. Ideally
        //        checking would continue, but the property would be labeled with an error.
        if let Get(_) = msg {
            let mut history = history.clone();
            let _ = history.on_invoke(src, RegisterOp::Read);
            Some(history)
        } else if let Put(_req_id, value) = msg {
            let mut history = history.clone();
            let _ = history.on_invoke(src, RegisterOp::Write(*value));
            Some(history)
        } else {
            None
        }
    }

    fn record_msg_in(&self, history: &Self::History, _src: Id, dst: Id, msg: &<Self::Actor as Actor>::Msg) -> Option<Self::History> {
        // FIXME: Currently throws away useful information about invalid histories. Ideally
        //        checking would continue, but the property would be labeled with an error.
        match msg {
            GetOk(_, v) => {
                let mut history = history.clone();
                let _ = history.on_return(dst, RegisterRet::ReadOk(*v));
                Some(history)
            }
            PutOk(_) => {
                let mut history = history.clone();
                let _ = history.on_return(dst, RegisterRet::WriteOk);
                Some(history)
            }
            _ => None
        }
    }

    fn properties(&self) -> Vec<Property<SystemModel<Self>>> {
        vec![
            Property::<SystemModel<Self>>::always("linearizable", |_, state| {
                state.history.serialized_history().is_some()
            }),
            Property::<SystemModel<Self>>::sometimes("value chosen",  |_, state| {
                for env in &state.network {
                    if let RegisterMsg::GetOk(_req_id, value) = env.msg {
                        if value != TestValue::default() { return true; }
                    }
                }
                false
            }),
        ]
    }

    fn within_boundary(&self, state: &SystemState<Self>) -> bool {
        (self.within_boundary)(state)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegisterActor<ServerActor> {
    /// A client that [`RegisterMsg::Put`]s a message and upon receving a
    /// corresponding [`RegisterMsg::PutOk`] follows up with a
    /// [`RegisterMsg::Get`].
    Client {
        server_count: u64,
    },
    /// A server actor being validated.
    Server(ServerActor),
}
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
#[derive(serde::Serialize)]
pub enum RegisterActorState<ServerState> {
    /// A client that sends a sequence of [`RegisterMsg::Put`] messages before sending a
    /// [`RegisterMsg::Get`].
    Client {
        awaiting: Option<TestRequestId>,
        op_count: u64,
    },
    /// Wraps the state of a server actor.
    Server(ServerState),
}

// This implementation assumes the servers are at the beginning of the list of
// actors in the system under test so that an arbitrary server destination ID
// can be derived from `(client_id.0 + k) % server_count` for any `k`.
impl<ServerActor, InternalMsg> Actor for RegisterActor<ServerActor>
where
    ServerActor: Actor<Msg = RegisterMsg<TestRequestId, TestValue, InternalMsg>>,
    InternalMsg: Clone + Debug + Eq + Hash,
{
    type Msg = RegisterMsg<TestRequestId, TestValue, InternalMsg>;
    type State = RegisterActorState<ServerActor::State>;

    #[allow(clippy::identity_op)]
    fn on_start(&self, id: Id, o: &mut Out<Self>) -> Self::State {
        match self {
            RegisterActor::Client { server_count } => {
                let index = id.0;
                let unique_request_id = 1 * index as TestRequestId; // next will be 2 * index
                let value = (b'A' + (index - server_count) as u8) as char;
                o.send(
                    Id((index + 0) % server_count),
                    Put(unique_request_id, value));
                RegisterActorState::Client {
                    awaiting: Some(unique_request_id),
                    op_count: 1,
                }
            }
            RegisterActor::Server(server_actor) => {
                let mut server_out = Out::new();
                let state = RegisterActorState::Server(server_actor.on_start(id, &mut server_out));
                o.append(&mut server_out);
                state
            }
        }
    }

    fn on_msg(&self, id: Id, state: &mut Cow<Self::State>, src: Id, msg: Self::Msg, o: &mut Out<Self>) {
        use RegisterActor as A;
        use RegisterActorState as S;

        match (self, &**state) {
            (A::Client { server_count }, S::Client {
                                             awaiting: Some(awaiting),
                                             op_count
                                         }) => {
                match msg {
                    RegisterMsg::PutOk(request_id) if &request_id == awaiting => {
                        // Clients send a sequence of `Put`s followed by a `Get`. As a simple
                        // heuristic to cover a wider range of behaviors: the first client's `Put`
                        // sequence is of length 2, while the others are of length 1.
                        let index = id.0;
                        let unique_request_id = ((op_count + 1) * index) as TestRequestId;
                        let max_put_count = if index == *server_count { 2 } else { 1 };
                        if *op_count < max_put_count {
                            let value = (b'Z' - (index - server_count) as u8) as char;
                            o.send(
                                Id((index + op_count) % server_count),
                                Put(unique_request_id, value));
                        } else {
                            o.send(
                                Id((index + op_count) % server_count),
                                Get(unique_request_id));
                        }
                        *state = Cow::Owned(RegisterActorState::Client {
                            awaiting: Some(unique_request_id),
                            op_count: op_count + 1,
                        });
                    }
                    RegisterMsg::GetOk(request_id, _value) if &request_id == awaiting => {
                        *state = Cow::Owned(RegisterActorState::Client {
                            awaiting: None,
                            op_count: op_count + 1,
                        });
                    }
                    _ => {}
                }
            }
            (A::Server(server_actor), S::Server(server_state)) => {
                let mut server_state = Cow::Borrowed(server_state);
                let mut server_out = Out::new();
                server_actor.on_msg(id, &mut server_state, src, msg, &mut server_out);
                if let Cow::Owned(server_state) = server_state {
                    *state = Cow::Owned(RegisterActorState::Server(server_state))
                }
                o.append(&mut server_out);
            }
            _ => {}
        }
    }
}


/// A simple request ID type for tests.
pub type TestRequestId = u64;

/// A simple value type for tests.
pub type TestValue = char;
