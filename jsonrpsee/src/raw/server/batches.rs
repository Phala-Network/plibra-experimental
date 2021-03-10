// Copyright 2019 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any
// person obtaining a copy of this software and associated
// documentation files (the "Software"), to deal in the
// Software without restriction, including without
// limitation the rights to use, copy, modify, merge,
// publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software
// is furnished to do so, subject to the following
// conditions:
//
// The above copyright notice and this permission notice
// shall be included in all copies or substantial portions
// of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF
// ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED
// TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A
// PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT
// SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
// CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION
// OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR
// IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::common;
use crate::raw::server::{batch, params::Params, Notification};

use alloc::vec::Vec;
use core::fmt;
use hashbrown::{hash_map::Entry, HashMap};

/// Collection of multiple batches.
///
/// This struct manages the state of the requests that have been received by the server and that
/// are waiting for a response. Due to the batching mechanism in the JSON-RPC protocol, one single
/// message can contain multiple requests and notifications that must all be answered at once.
///
/// # Usage
///
/// - Create a new empty [`BatchesState`] with [`new`](BatchesState::new).
/// - Whenever the server receives a JSON message, call [`inject`](BatchesState::inject).
/// - Call [`next_event`](BatchesState::next_event) in a loop and process the events buffered
/// within the object.
///
/// The [`BatchesState`] also acts as a collection of pending requests, which you can query using
/// [`request_by_id`](BatchesState::request_by_id).
///
pub struct BatchesState<T> {
    /// Identifier of the next batch to add to `batches`.
    next_batch_id: u64,

    /// For each batch, the individual batch's state and the user parameter.
    ///
    /// The identifier is lineraly increasing and is never leaked on the wire or outside of this
    /// module. Therefore there is no risk of hash collision.
    batches: HashMap<u64, (batch::BatchState, T), fnv::FnvBuildHasher>,
}

/// Event generated by [`next_event`](BatchesState::next_event).
#[derive(Debug)]
pub enum BatchesEvent<'a, T> {
    /// A notification has been extracted from a batch.
    Notification {
        /// Notification in question.
        notification: Notification,
        /// User parameter passed when calling [`inject`](BatchesState::inject).
        user_param: &'a mut T,
    },

    /// A request has been extracted from a batch.
    Request(BatchesElem<'a, T>),

    /// A batch has gotten all its requests answered and a response is ready to be sent out.
    ReadyToSend {
        /// Response to send out to the JSON-RPC client.
        response: common::Response,
        /// User parameter passed when calling [`inject`](BatchesState::inject).
        user_param: T,
    },
}

/// Request within the batches.
pub struct BatchesElem<'a, T> {
    /// Id of the batch that contains this element.
    batch_id: u64,
    /// Inner reference to a request within a batch.
    inner: batch::BatchElem<'a>,
    /// User parameter passed when calling `inject`.
    user_param: &'a mut T,
}

/// Identifier of a request within a [`BatchesState`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BatchesElemId {
    /// Id of the batch within `BatchesState::batches`.
    outer: u64,
    /// Id of the request within the batch.
    inner: usize,
}

/// Minimal capacity for the `batches` container.
const BATCHES_MIN_CAPACITY: usize = 256;

impl<T> BatchesState<T> {
    /// Creates a new empty `BatchesState`.
    pub fn new() -> BatchesState<T> {
        BatchesState {
            next_batch_id: 0,
            batches: HashMap::with_capacity_and_hasher(BATCHES_MIN_CAPACITY, Default::default()),
        }
    }

    /// Processes one step from a batch and returns an event. Returns `None` if there is nothing
    /// to do. After you call `inject`, then this will return `Some` at least once.
    pub fn next_event(&mut self) -> Option<BatchesEvent<T>> {
        // Note that this function has a complexity of `O(n)`, as we iterate over every single
        // batch every single time. This is however the most straight-forward way to implement it,
        // and while better strategies might yield better complexities, it might not actually yield
        // better performances in real-world situations. More brainstorming and benchmarking could
        // get helpful here.

        // Because of long-standing Rust lifetime issues
        // (https://github.com/rust-lang/rust/issues/51526), we can't do this in an elegant way.
        // If you're reading this code, know that it took several iterations and that I hated my
        // life while trying to figure out how to make the compiler happy.
        for batch_id in self.batches.keys().cloned().collect::<Vec<_>>() {
            enum WhatCanWeDo {
                Nothing,
                ReadyToRespond,
                Notification(Notification),
                Request(usize),
            }

            let what_can_we_do = {
                let (batch, _) = self
                    .batches
                    .get_mut(&batch_id)
                    .expect("all keys are valid; qed");
                let is_ready_to_respond = batch.is_ready_to_respond();
                match batch.next() {
                    None if is_ready_to_respond => WhatCanWeDo::ReadyToRespond,
                    None => WhatCanWeDo::Nothing,
                    Some(batch::BatchInc::Notification(n)) => WhatCanWeDo::Notification(n),
                    Some(batch::BatchInc::Request(inner)) => WhatCanWeDo::Request(inner.id()),
                }
            };

            match what_can_we_do {
                WhatCanWeDo::Nothing => {}
                WhatCanWeDo::ReadyToRespond => {
                    let (batch, user_param) = self
                        .batches
                        .remove(&batch_id)
                        .expect("key was grabbed from self.batches; qed");
                    let response = batch
                        .into_response()
                        .unwrap_or_else(|_| panic!("is_ready_to_respond returned true; qed"));
                    if let Some(response) = response {
                        return Some(BatchesEvent::ReadyToSend {
                            response,
                            user_param,
                        });
                    }
                }
                WhatCanWeDo::Notification(notification) => {
                    return Some(BatchesEvent::Notification {
                        notification,
                        user_param: &mut self.batches.get_mut(&batch_id).unwrap().1,
                    });
                }
                WhatCanWeDo::Request(id) => {
                    let (batch, user_param) = self.batches.get_mut(&batch_id).unwrap();
                    return Some(BatchesEvent::Request(BatchesElem {
                        batch_id,
                        inner: batch.request_by_id(id).unwrap(),
                        user_param,
                    }));
                }
            }
        }

        None
    }

    /// Injects a newly-received batch into the list. You must then call
    /// [`next_event`](BatchesState::next_event) in order to process it.
    pub fn inject(&mut self, request: common::Request, user_param: T) {
        let batch = batch::BatchState::from_request(request);

        loop {
            let id = self.next_batch_id;
            self.next_batch_id = self.next_batch_id.wrapping_add(1);

            // We shrink `self.batches` from time to time so that it doesn't grow too much.
            if id % 256 == 0 {
                self.batches.shrink_to_fit();
                // TODO: self.batches.shrink_to(BATCHES_MIN_CAPACITY);
                // ^ see https://github.com/rust-lang/rust/issues/56431
            }

            match self.batches.entry(id) {
                Entry::Occupied(_) => continue,
                Entry::Vacant(e) => {
                    e.insert((batch, user_param));
                    break;
                }
            }
        }
    }

    /// Returns a list of all user data associated to active batches.
    pub fn batches<'a>(&'a mut self) -> impl Iterator<Item = &'a mut T> + 'a {
        self.batches.values_mut().map(|(_, user_data)| user_data)
    }

    /// Returns a request previously returned by [`next_event`](crate::RawServer::next_event) by its
    /// id.
    ///
    /// Note that previous notifications don't have an ID and can't be accessed with this method.
    ///
    /// Returns `None` if the request ID is invalid or if the request has already been answered in
    /// the past.
    pub fn request_by_id(&mut self, id: BatchesElemId) -> Option<BatchesElem<T>> {
        if let Some((batch, user_param)) = self.batches.get_mut(&id.outer) {
            Some(BatchesElem {
                batch_id: id.outer,
                inner: batch.request_by_id(id.inner)?,
                user_param,
            })
        } else {
            None
        }
    }
}

impl<T> Default for BatchesState<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> fmt::Debug for BatchesState<T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_list().entries(self.batches.values()).finish()
    }
}

impl<'a, T> BatchesElem<'a, T> {
    /// Returns the id of the request within the [`BatchesState`].
    ///
    /// > **Note**: This is NOT the request id that the client passed.
    pub fn id(&self) -> BatchesElemId {
        BatchesElemId {
            outer: self.batch_id,
            inner: self.inner.id(),
        }
    }

    /// Returns the user parameter passed when calling [`inject`](BatchesState::inject).
    pub fn user_param(&mut self) -> &mut T {
        &mut self.user_param
    }

    /// Returns the id that the client sent out.
    pub fn request_id(&self) -> &common::Id {
        self.inner.request_id()
    }

    /// Returns the method of this request.
    pub fn method(&self) -> &str {
        self.inner.method()
    }

    /// Returns the parameters of the request, as a `common::Params`.
    pub fn params(&self) -> Params {
        self.inner.params()
    }

    /// Responds to the request. This destroys the request object, meaning you can no longer
    /// retrieve it with [`request_by_id`](BatchesState::request_by_id) later anymore.
    ///
    /// A [`ReadyToSend`](BatchesEvent::ReadyToSend) event containing this response might be
    /// generated the next time you call [`next_event`](BatchesState::next_event).
    pub fn set_response(self, response: Result<common::JsonValue, common::Error>) {
        self.inner.set_response(response)
    }
}

impl<'a, T> fmt::Debug for BatchesElem<'a, T>
where
    T: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("BatchesElem")
            .field("id", &self.id())
            .field("user_param", &self.user_param)
            .field("request_id", &self.request_id())
            .field("method", &self.method())
            .field("params", &self.params())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{BatchesEvent, BatchesState};
    use crate::{common, raw::server::Notification};

    #[test]
    fn basic_notification() {
        let notif = common::Notification {
            jsonrpc: common::Version::V2,
            method: "foo".to_string(),
            params: common::Params::None,
        };

        let mut state = BatchesState::new();
        assert!(state.next_event().is_none());
        state.inject(
            common::Request::Single(common::Call::Notification(notif.clone())),
            (),
        );
        match state.next_event() {
            Some(BatchesEvent::Notification {
                ref notification, ..
            }) if *notification == Notification::from(notif) => {}
            _ => panic!(),
        }
        assert!(state.next_event().is_none());
    }

    #[test]
    fn basic_request() {
        let call = common::MethodCall {
            jsonrpc: common::Version::V2,
            method: "foo".to_string(),
            params: common::Params::Map(serde_json::from_str("{\"test\":\"foo\"}").unwrap()),
            id: common::Id::Num(123),
        };

        let mut state = BatchesState::new();
        assert!(state.next_event().is_none());
        state.inject(
            common::Request::Single(common::Call::MethodCall(call)),
            8889,
        );

        let rq_id = match state.next_event() {
            Some(BatchesEvent::Request(rq)) => {
                assert_eq!(rq.method(), "foo");
                assert_eq!(
                    {
                        let v: String = rq.params().get("test").unwrap();
                        v
                    },
                    "foo"
                );
                assert_eq!(rq.request_id(), &common::Id::Num(123));
                rq.id()
            }
            _ => panic!(),
        };

        assert!(state.next_event().is_none());

        assert_eq!(state.request_by_id(rq_id).unwrap().method(), "foo");
        state
            .request_by_id(rq_id)
            .unwrap()
            .set_response(Err(common::Error::method_not_found()));
        assert!(state.request_by_id(rq_id).is_none());

        match state.next_event() {
            Some(BatchesEvent::ReadyToSend {
                response,
                user_param,
            }) => {
                assert_eq!(user_param, 8889);
                match response {
                    common::Response::Single(common::Output::Failure(f)) => {
                        assert_eq!(f.id, common::Id::Num(123));
                    }
                    _ => panic!(),
                }
            }
            _ => panic!(),
        };
    }

    #[test]
    fn empty_batch() {
        let mut state = BatchesState::new();
        assert!(state.next_event().is_none());
        state.inject(common::Request::Batch(Vec::new()), ());
        assert!(state.next_event().is_none());
    }

    #[test]
    fn batch_of_notifs() {
        let notif1 = common::Notification {
            jsonrpc: common::Version::V2,
            method: "foo".to_string(),
            params: common::Params::None,
        };

        let notif2 = common::Notification {
            jsonrpc: common::Version::V2,
            method: "bar".to_string(),
            params: common::Params::None,
        };

        let mut state = BatchesState::new();
        assert!(state.next_event().is_none());
        state.inject(
            common::Request::Batch(vec![
                common::Call::Notification(notif1.clone()),
                common::Call::Notification(notif2.clone()),
            ]),
            2,
        );

        match state.next_event() {
            Some(BatchesEvent::Notification {
                ref notification,
                ref user_param,
            }) if *notification == Notification::from(notif1) && **user_param == 2 => {}
            _ => panic!(),
        }

        match state.next_event() {
            Some(BatchesEvent::Notification {
                ref notification,
                ref user_param,
            }) if *notification == Notification::from(notif2) && **user_param == 2 => {}
            _ => panic!(),
        }

        assert!(state.next_event().is_none());
    }
}
