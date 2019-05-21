use std::marker::PhantomData;

use actix_codec::{AsyncRead, AsyncWrite, Framed};
use actix_server_config::{Io as IoStream, ServerConfig};
use actix_service::{boxed, boxed::BoxedService, IntoService, NewService, Service};
use amqp_codec::protocol::{Error, Frame, ProtocolId};
use amqp_codec::{AmqpCodec, AmqpFrame, ProtocolIdCodec, ProtocolIdError, SaslFrame};
use futures::future::{err, ok, Either, FutureResult, IntoFuture};
use futures::{Async, Future, Poll, Sink, Stream};

use crate::cell::Cell;
use crate::connection::Connection;
use crate::Configuration;

use super::dispatcher::Dispatcher;
use super::errors::HandshakeError;
use super::link::Link;
use super::sasl::{Sasl, SaslAuth};
use super::state::State;

/// Server dispatcher factory
pub struct Server<Io, F, St, S, P> {
    inner: Cell<Inner<Io, F, St, S, P>>,
}

pub(super) struct Inner<Io, F, St, S, P> {
    pub factory: F,
    config: Configuration,
    disconnect: Option<BoxedService<State<St>, (), ()>>,
    _t: PhantomData<(Io, St, S, P)>,
}

impl<Io, F, St, S, P> Server<Io, F, St, S, P>
where
    Io: AsyncRead + AsyncWrite,
    F: Service<Request = (Option<SaslAuth>, P), Response = (St, S), Error = Error> + 'static,
    S: Service<Request = Link<St>, Response = (), Error = Error>,
{
    /// Create server dispatcher factory
    pub fn new(config: Configuration, factory: F) -> Self {
        Self {
            inner: Cell::new(Inner {
                factory,
                config,
                disconnect: None,
                _t: PhantomData,
            }),
        }
    }

    /// Service to call on disconnect
    pub fn disconnect<UF, U>(self, srv: UF) -> Self
    where
        UF: IntoService<U>,
        U: Service<Request = State<St>, Response = (), Error = ()> + 'static,
    {
        self.inner.get_mut().disconnect = Some(boxed::service(srv.into_service()));
        self
    }
}

impl<Io, F, St, S, P> Clone for Server<Io, F, St, S, P> {
    fn clone(&self) -> Self {
        Server {
            inner: self.inner.clone(),
        }
    }
}

impl<Io, F, St, S, P> NewService for Server<Io, F, St, S, P>
where
    Io: AsyncRead + AsyncWrite + 'static,
    F: Service<Request = (Option<SaslAuth>, P), Response = (St, S), Error = Error> + 'static,
    S: Service<Request = Link<St>, Response = (), Error = Error> + 'static,
    St: 'static,
    P: 'static,
{
    type Config = ServerConfig;
    type Request = IoStream<Io, P>;
    type Response = ();
    type Error = ();
    type Service = ServerService<Io, F, St, S, P>;
    type InitError = ();
    type Future = FutureResult<Self::Service, Self::InitError>;

    fn new_service(&self, _: &ServerConfig) -> Self::Future {
        ok(ServerService {
            inner: self.inner.clone(),
        })
    }
}

/// Server dispatcher
pub struct ServerService<Io, F, St, S, P> {
    inner: Cell<Inner<Io, F, St, S, P>>,
}

impl<Io, F, St, S, P> Service for ServerService<Io, F, St, S, P>
where
    Io: AsyncRead + AsyncWrite + 'static,
    F: Service<Request = (Option<SaslAuth>, P), Response = (St, S), Error = Error> + 'static,
    S: Service<Request = Link<St>, Response = (), Error = Error> + 'static,
    St: 'static,
    P: 'static,
{
    type Request = IoStream<Io, P>;
    type Response = ();
    type Error = ();
    type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

    fn poll_ready(&mut self) -> Poll<(), Self::Error> {
        Ok(Async::Ready(()))
    }

    fn call(&mut self, req: Self::Request) -> Self::Future {
        let (req, param, _) = req.into_parts();

        let inner = self.inner.clone();
        let inner2 = self.inner.clone();
        Box::new(
            Framed::new(req, ProtocolIdCodec)
                .into_future()
                .map_err(|e| HandshakeError::from(e.0))
                .and_then(move |(protocol, framed)| match protocol {
                    Some(ProtocolId::Amqp) => {
                        let inner = inner;
                        Either::A(
                            framed
                                .send(ProtocolId::Amqp)
                                .map_err(|e| HandshakeError::from(e))
                                .and_then(move |framed| {
                                    let framed = framed.into_framed(AmqpCodec::new());
                                    open_connection(inner.config.clone(), framed).and_then(
                                        move |conn| {
                                            inner
                                                .get_mut()
                                                .factory
                                                .call((None, param))
                                                .map_err(|_| HandshakeError::Service)
                                                .map(move |(st, srv)| (st, srv, conn))
                                        },
                                    )
                                }),
                        )
                    }
                    Some(ProtocolId::AmqpSasl) => {
                        let mut inner = inner;
                        Either::B(Either::A(
                            framed
                                .send(ProtocolId::AmqpSasl)
                                .map_err(|e| HandshakeError::from(e))
                                .and_then(move |framed| {
                                    Sasl::new(
                                        param,
                                        &mut inner,
                                        framed.into_framed(AmqpCodec::<SaslFrame>::new()),
                                    )
                                    .and_then(
                                        move |(st, srv, framed)| {
                                            let framed = framed.into_framed(ProtocolIdCodec);
                                            handshake(inner.config.clone(), framed)
                                                .map(move |conn| (st, srv, conn))
                                        },
                                    )
                                }),
                        ))
                    }
                    Some(ProtocolId::AmqpTls) => Either::B(Either::B(err(HandshakeError::from(
                        ProtocolIdError::Unexpected {
                            exp: ProtocolId::Amqp,
                            got: ProtocolId::AmqpTls,
                        },
                    )))),
                    None => Either::B(Either::B(err(HandshakeError::Disconnected.into()))),
                })
                .map_err(|_| ())
                .and_then(move |(st, srv, conn)| {
                    let st = Cell::new(st);
                    let state = State::new(st.clone());
                    let inner = inner2.clone();
                    Dispatcher::new(conn, st, srv).then(move |res| {
                        if inner.disconnect.is_some() {
                            Either::A(
                                inner
                                    .get_mut()
                                    .disconnect
                                    .as_mut()
                                    .unwrap()
                                    .call(state)
                                    .then(move |_| res),
                            )
                        } else {
                            Either::B(res.into_future())
                        }
                    })
                }),
        )
    }
}

pub fn handshake<Io>(
    cfg: Configuration,
    framed: Framed<Io, ProtocolIdCodec>,
) -> impl Future<Item = Connection<Io>, Error = HandshakeError>
where
    Io: AsyncRead + AsyncWrite + 'static,
{
    framed
        .into_future()
        .map_err(|e| HandshakeError::from(e.0))
        .and_then(move |(protocol, framed)| {
            if let Some(protocol) = protocol {
                if protocol == ProtocolId::Amqp {
                    Ok(framed)
                } else {
                    Err(ProtocolIdError::Unexpected {
                        exp: ProtocolId::Amqp,
                        got: protocol,
                    }
                    .into())
                }
            } else {
                Err(ProtocolIdError::Disconnected.into())
            }
        })
        .and_then(move |framed| {
            framed
                .send(ProtocolId::Amqp)
                .map_err(HandshakeError::from)
                .map(|framed| framed.into_framed(AmqpCodec::new()))
        })
        .and_then(move |framed| open_connection(cfg.clone(), framed))
}

pub fn open_connection<Io>(
    cfg: Configuration,
    framed: Framed<Io, AmqpCodec<AmqpFrame>>,
) -> impl Future<Item = Connection<Io>, Error = HandshakeError>
where
    Io: AsyncRead + AsyncWrite + 'static,
{
    // read Open frame
    framed
        .into_future()
        .map_err(|res| HandshakeError::from(res.0))
        .and_then(|(frame, framed)| {
            if let Some(frame) = frame {
                let frame = frame.into_parts().1;
                match frame {
                    Frame::Open(open) => {
                        trace!("Got open: {:?}", open);
                        Ok((open, framed))
                    }
                    frame => Err(HandshakeError::Unexpected(frame)),
                }
            } else {
                Err(HandshakeError::Disconnected)
            }
        })
        .and_then(move |(open, framed)| {
            // confirm Open
            let local = cfg.to_open(None);
            framed
                .send(AmqpFrame::new(0, local.into()))
                .map_err(HandshakeError::from)
                .map(move |framed| Connection::new(framed, cfg.clone(), (&open).into(), None))
        })
}
