//! Worker thread — selects on call and cast channels, processes messages.

use flume::{Receiver, Sender, TrySendError};

use crate::ffi::{CallMessage, CastMessage, EventBody, EventMessage, MessageBody, ResponseMessage};

pub fn run(
    call_rx: Receiver<CallMessage>,
    cast_rx: Receiver<CastMessage>,
    response_tx: Sender<ResponseMessage>,
    event_tx: Sender<EventMessage>,
) {
    let mut call_alive = true;
    let mut cast_alive = true;

    while call_alive || cast_alive {
        let mut sel = flume::Selector::new();

        if call_alive {
            sel = sel.recv(&call_rx, |result| match result {
                Ok(CallMessage { token, .. }) => {
                    let msg = ResponseMessage {
                        token,
                        body: MessageBody::Pong,
                    };
                    response_tx.send(msg).expect("send response to dispatch");
                }
                Err(_) => call_alive = false,
            });
        }

        if cast_alive {
            sel = sel.recv(&cast_rx, |result| match result {
                Ok(CastMessage { .. }) => {
                    let event = EventMessage { body: EventBody::CastReceived };
                    if let Err(TrySendError::Full(_)) = event_tx.try_send(event) {
                        eprintln!("warning: event channel full, dropping CastReceived event");
                    }
                }
                Err(_) => cast_alive = false,
            });
        }

        sel.wait();
    }
}
