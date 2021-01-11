use std::fmt;

use ntex_amqp_codec::protocol;

use crate::cell::Cell;
use crate::error::AmqpProtocolError;
use crate::rcvlink::ReceiverLink;
use crate::session::SessionInner;
use crate::sndlink::SenderLink;

pub struct ControlFrame(pub(super) Cell<FrameInner>);

pub(super) struct FrameInner {
    pub(super) kind: ControlFrameKind,
    pub(super) session: Option<Cell<SessionInner>>,
}

impl fmt::Debug for ControlFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlFrame")
            .field("kind", &self.0.get_ref().kind)
            .finish()
    }
}

#[derive(Debug)]
pub enum ControlFrameKind {
    AttachReceiver(ReceiverLink),
    AttachSender(Box<protocol::Attach>, SenderLink),
    Flow(protocol::Flow, SenderLink),
    DetachSender(protocol::Detach, SenderLink),
    DetachReceiver(protocol::Detach, ReceiverLink),
    ProtocolError(AmqpProtocolError),
    Closed(bool),
}

impl ControlFrame {
    pub(crate) fn new(session: Cell<SessionInner>, kind: ControlFrameKind) -> Self {
        ControlFrame(Cell::new(FrameInner {
            session: Some(session),
            kind,
        }))
    }

    pub(crate) fn new_kind(kind: ControlFrameKind) -> Self {
        ControlFrame(Cell::new(FrameInner {
            session: None,
            kind,
        }))
    }

    pub(crate) fn clone(&self) -> Self {
        ControlFrame(self.0.clone())
    }

    pub(crate) fn session(&self) -> &Cell<SessionInner> {
        self.0.get_ref().session.as_ref().unwrap()
    }

    #[inline]
    pub fn frame(&self) -> &ControlFrameKind {
        &self.0.kind
    }
}