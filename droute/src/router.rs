// Copyright 2020 LEXUGE
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Router is the core concept of `droute`.

pub mod filter;
pub mod matcher;
pub mod upstream;

use self::{
    filter::{Filter, Rule},
    matcher::Matcher,
    upstream::{Upstream, Upstreams},
};
use crate::error::Result;
use lazy_static::lazy_static;
use log::warn;
use std::{
    fmt::{Debug, Display},
    hash::Hash,
};
use trust_dns_client::{
    op::{Message, ResponseCode},
    rr::{rdata::soa::SOA, record_data::RData, resource::Record, Name, RecordType},
};

// Maximum TTL as defined in https://tools.ietf.org/html/rfc2181, 2147483647
//   Setting this to a value of 1 day, in seconds
pub(self) const MAX_TTL: u32 = 86400_u32;

// Data from smartdns. https://github.com/pymumu/smartdns/blob/42b3e98b2a3ca90ea548f8cb5ed19a3da6011b74/src/dns_server.c#L651
lazy_static! {
    static ref SOA_RDATA: RData = {
        RData::SOA(SOA::new(
            Name::from_utf8("a.gtld-servers.net").unwrap(),
            Name::from_utf8("nstld.verisign-grs.com").unwrap(),
            1800,
            1800,
            900,
            604800,
            86400,
        ))
    };
}

/// Router implementation.
/// `'static + Send + Sync` is required for async usages.
/// `Display + Debug` is required for Error formatting implementation (It is intuitive for you to have your label readable).
/// `Eq + Clone + Hash` is required for internal design.
pub struct Router<L, M> {
    filter: Filter<L, M>,
    disable_ipv6: bool,
    upstreams: Upstreams<L>,
}

impl<L, M: Matcher<Label = L>> Router<L, M>
where
    L: 'static + Display + Debug + Eq + Hash + Send + Clone + Sync,
{
    /// Create a new `Router` from configuration and check the validity. `data` is the content of the configuration file.
    pub async fn new(
        upstreams: Vec<Upstream<L>>,
        disable_ipv6: bool,
        cache_size: usize,
        default_tag: L,
        rules: Vec<Rule<L>>,
    ) -> Result<L, Self> {
        let filter = Filter::new(default_tag, rules).await?;
        let router = Self {
            disable_ipv6,
            upstreams: Upstreams::new(upstreams, cache_size).await?,
            filter,
        };
        router.check()?;
        Ok(router)
    }

    /// Validate the internal rules defined. This is automatically performed by `new` method.
    pub fn check(&self) -> Result<L, bool> {
        self.upstreams.hybrid_check()?;
        for dst in self.filter.get_dsts() {
            self.upstreams.exists(dst)?;
        }
        self.upstreams.exists(&self.filter.default_tag())?;
        Ok(true)
    }

    /// Resolve the DNS query with routing rules defined.
    pub async fn resolve(&self, mut msg: Message) -> Result<L, Message> {
        let (id, op_code) = (msg.id(), msg.op_code());
        let tag = if msg.query_count() == 1 {
            let q = msg.queries().iter().next().unwrap(); // Safe unwrap here because query_count == 1
            if (q.query_type() == RecordType::AAAA) && (self.disable_ipv6) {
                // If `disable_ipv6` has been set, return immediately SOA.
                return Ok({
                    let r = Record::from_rdata(q.name().clone(), MAX_TTL, SOA_RDATA.clone());
                    // We can't add record to authority section but somehow it works
                    msg.add_additional(r);
                    msg
                });
            } else {
                self.filter.get_upstream(q.name().to_utf8().as_str())
            }
        } else {
            warn!("DNS message contains multiple/zero querie(s), using default_tag to route. IPv6 disable functionality is NOT taking effect.");
            self.filter.default_tag()
        };
        Ok(match self.upstreams.resolve(tag, &msg).await {
            Ok(m) => m,
            Err(e) => {
                // Catch all server failure here and return server fail
                warn!("Upstream encountered error: {}, returning SERVFAIL", e);
                Message::error_msg(id, op_code, ResponseCode::ServFail)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        upstream::{Upstream, UpstreamKind::Udp},
        Router,
    };
    use dmatcher::{domain::Domain, Label};
    use lazy_static::lazy_static;
    use std::net::SocketAddr;
    use tokio::net::UdpSocket;
    use trust_dns_client::op::Message;
    use trust_dns_proto::{
        op::{header::MessageType, query::Query},
        rr::{record_data::RData, record_type::RecordType, resource::Record, Name},
    };

    lazy_static! {
        static ref DUMMY_MSG: Message = {
            let mut msg = Message::new();
            msg.add_answer(Record::from_rdata(
                Name::from_utf8("www.apple.com").unwrap(),
                32,
                RData::A("1.1.1.1".parse().unwrap()),
            ));
            msg.set_message_type(MessageType::Response);
            msg
        };
        static ref QUERY: Message = {
            let mut msg = Message::new();
            msg.add_query(Query::query(
                Name::from_utf8("www.apple.com").unwrap(),
                RecordType::A,
            ));
            msg
        };
    }

    struct Server {
        socket: UdpSocket,
        buf: Vec<u8>,
        to_send: Option<SocketAddr>,
    }

    impl Server {
        async fn run(self) -> Result<(), std::io::Error> {
            let Server {
                socket,
                mut buf,
                mut to_send,
            } = self;

            loop {
                // First we check to see if there's a message we need to echo back.
                // If so then we try to send it back to the original source, waiting
                // until it's writable and we're able to do so.
                if let Some(peer) = to_send {
                    // ID is required to match for trust-dns-client to accept response
                    let id = Message::from_vec(&buf).unwrap().id();
                    socket
                        .send_to(&DUMMY_MSG.clone().set_id(id).to_vec().unwrap(), &peer)
                        .await?;
                }

                // If we're here then `to_send` is `None`, so we take a look for the
                // next message we're going to echo back.
                to_send = Some(socket.recv_from(&mut buf).await?.1);
            }
        }
    }

    #[tokio::test]
    async fn test_resolve() {
        let socket = UdpSocket::bind(&"127.0.0.1:53533").await.unwrap();
        let server = Server {
            socket,
            buf: vec![0; 1024],
            to_send: None,
        };
        tokio::spawn(server.run());

        let router: Router<Label, Domain<Label>> = Router::new(
            vec![Upstream {
                timeout: 10,
                method: Udp("127.0.0.1:53533".parse().unwrap()),
                tag: "mock".into(),
            }],
            true,
            0,
            "mock".into(),
            vec![],
        )
        .await
        .unwrap();

        assert_eq!(
            router.resolve(QUERY.clone()).await.unwrap().answers(),
            DUMMY_MSG.answers()
        );
    }
}
