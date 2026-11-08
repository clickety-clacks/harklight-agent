use std::{
    io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    sync::Arc,
};

use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

type Lookup = dyn Fn(&str) -> io::Result<Vec<SocketAddr>> + Send + Sync;

/// Resolves the configured connect address whenever the HTTP client opens a
/// new connection. The URL host remains the independent TLS name.
pub(crate) struct ConnectAddressResolver {
    address: String,
    tls_name: String,
    lookup: Arc<Lookup>,
}

impl ConnectAddressResolver {
    pub(crate) fn new(address: impl Into<String>, tls_name: impl Into<String>) -> Self {
        let tls_name = tls_name.into();
        let normalized_tls_name = Url::parse(&format!("https://{tls_name}/"))
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or(tls_name);
        Self {
            address: address.into(),
            tls_name: normalized_tls_name,
            lookup: Arc::new(resolve_configured_address),
        }
    }

    #[cfg(test)]
    fn with_lookup(
        address: impl Into<String>,
        tls_name: impl Into<String>,
        lookup: Arc<Lookup>,
    ) -> Self {
        let tls_name = tls_name.into();
        let normalized_tls_name = Url::parse(&format!("https://{tls_name}/"))
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned))
            .unwrap_or(tls_name);
        Self {
            address: address.into(),
            tls_name: normalized_tls_name,
            lookup,
        }
    }
}

impl Resolve for ConnectAddressResolver {
    fn resolve(&self, _name: Name) -> Resolving {
        let address = self.address.clone();
        let tls_name = self.tls_name.clone();
        let lookup = Arc::clone(&self.lookup);
        Box::pin(async move {
            let lookup_address = if _name.as_str().eq_ignore_ascii_case(&tls_name) {
                address.as_str()
            } else {
                _name.as_str()
            };
            let addresses = lookup(lookup_address).map_err(|error| {
                Box::new(io::Error::new(
                    error.kind(),
                    format!("could not resolve address {lookup_address}: {error}"),
                )) as Box<dyn std::error::Error + Send + Sync>
            })?;
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

/// Uses the platform's standard resolver and leaves DNS names uncached here.
pub(crate) fn resolve_configured_address(address: &str) -> io::Result<Vec<SocketAddr>> {
    let addresses = if let Ok(ip) = address.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, 0)]
    } else {
        (address, 0).to_socket_addrs()?.collect()
    };
    if addresses.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "the platform resolver returned no addresses",
        ));
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        net::{Ipv4Addr, SocketAddr},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        task::{Context, Poll, Waker},
    };

    use reqwest::dns::{Name, Resolve};

    use super::ConnectAddressResolver;

    fn resolved_addresses(resolver: &ConnectAddressResolver) -> Result<Vec<SocketAddr>, String> {
        let mut future = resolver.resolve("harklight.example".parse::<Name>().unwrap());
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(Ok(addresses)) => Ok(addresses.collect()),
            Poll::Ready(Err(error)) => Err(error.to_string()),
            Poll::Pending => panic!("platform resolver future unexpectedly suspended"),
        }
    }

    #[test]
    fn configured_name_is_looked_up_again_for_each_new_connection() {
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let resolver = ConnectAddressResolver::with_lookup(
            "service.example",
            "harklight.example",
            Arc::new(move |_| {
                let call = observed.fetch_add(1, Ordering::SeqCst);
                let address = if call == 0 {
                    Ipv4Addr::new(192, 0, 2, 10)
                } else {
                    Ipv4Addr::new(192, 0, 2, 11)
                };
                Ok(vec![SocketAddr::new(address.into(), 0)])
            }),
        );

        assert_eq!(
            resolved_addresses(&resolver).unwrap()[0].ip(),
            "192.0.2.10".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(
            resolved_addresses(&resolver).unwrap()[0].ip(),
            "192.0.2.11".parse::<std::net::IpAddr>().unwrap()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn resolver_error_keeps_the_configured_name_in_its_message() {
        let resolver = ConnectAddressResolver::with_lookup(
            "no-such-name.invalid",
            "harklight.example",
            Arc::new(|_| Err(io::Error::new(io::ErrorKind::NotFound, "not found"))),
        );

        let error = resolved_addresses(&resolver).unwrap_err();
        assert!(error.contains("no-such-name.invalid"));
    }
}
