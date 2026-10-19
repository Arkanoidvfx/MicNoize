use std::time::Duration;

use ureq::unversioned::resolver::DefaultResolver;
use ureq::unversioned::transport::{
    Buffers, ConnectionDetails, Connector, DefaultConnector, NextTimeout, Transport,
};
use ureq::{Agent, Error, Timeout};

pub(crate) fn agent() -> Agent {
    let config = Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .build();
    Agent::with_parts(
        config,
        IdleConnector::new(Duration::from_secs(120)),
        DefaultResolver::default(),
    )
}

// ureq's body timeout covers the whole download; components need a per-read limit.
#[derive(Debug)]
struct IdleConnector {
    inner: DefaultConnector,
    idle: Duration,
}

impl IdleConnector {
    fn new(idle: Duration) -> Self {
        Self {
            inner: DefaultConnector::default(),
            idle,
        }
    }
}

impl Connector for IdleConnector {
    type Out = IdleTransport;

    fn connect(
        &self,
        details: &ConnectionDetails,
        chained: Option<()>,
    ) -> Result<Option<Self::Out>, Error> {
        Ok(self
            .inner
            .connect(details, chained)?
            .map(|inner| IdleTransport {
                inner,
                idle: self.idle,
            }))
    }
}

#[derive(Debug)]
struct IdleTransport {
    inner: Box<dyn Transport>,
    idle: Duration,
}

impl Transport for IdleTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        self.inner.buffers()
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
        let timeout = if timeout.after > self.idle.into() {
            NextTimeout {
                after: self.idle.into(),
                reason: Timeout::RecvBody,
            }
        } else {
            timeout
        };
        self.inner.await_input(timeout)
    }

    fn is_open(&mut self) -> bool {
        self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};
    use std::thread::{self, JoinHandle};
    use std::time::Instant;
    use ureq::unversioned::transport::{LazyBuffers, time};

    fn test_agent(idle: Duration, response: Duration, proxy: Option<ureq::Proxy>) -> Agent {
        let config = Agent::config_builder()
            .proxy(proxy)
            .timeout_connect(Some(Duration::from_secs(2)))
            .timeout_recv_response(Some(response))
            .build();
        Agent::with_parts(config, IdleConnector::new(idle), DefaultResolver::default())
    }

    fn loopback(handler: impl FnOnce(TcpStream) + Send + 'static) -> (String, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let thread = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            handler(stream);
        });
        (url, thread)
    }

    fn request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() < 8192);
        }
        String::from_utf8(bytes).unwrap()
    }

    #[test]
    fn idle_body_read_times_out_and_a_new_request_can_succeed() {
        let (url, server) = loopback(|mut stream| {
            request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nx")
                .unwrap();
            let _ = stream.read(&mut [0]);
        });
        let client = test_agent(Duration::from_millis(100), Duration::from_secs(2), None);
        let mut response = client.get(&url).call().unwrap();
        let start = Instant::now();
        assert!(matches!(
            response.body_mut().read_to_vec(),
            Err(Error::Timeout(Timeout::RecvBody))
        ));
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(response);
        server.join().unwrap();

        let (url, server) = loopback(|mut stream| {
            request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });
        assert_eq!(
            client
                .get(&url)
                .call()
                .unwrap()
                .body_mut()
                .read_to_vec()
                .unwrap(),
            b"ok"
        );
        server.join().unwrap();
    }

    #[test]
    fn a_slow_continuous_body_can_outlast_the_idle_limit() {
        let (url, server) = loopback(|mut stream| {
            request(&mut stream);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\n\r\n")
                .unwrap();
            for _ in 0..8 {
                thread::sleep(Duration::from_millis(100));
                stream.write_all(b"x").unwrap();
            }
        });
        let idle = Duration::from_millis(500);
        let client = test_agent(idle, Duration::from_secs(2), None);
        let start = Instant::now();
        assert_eq!(
            client
                .get(&url)
                .call()
                .unwrap()
                .body_mut()
                .read_to_vec()
                .unwrap(),
            b"xxxxxxxx"
        );
        assert!(start.elapsed() > idle);
        server.join().unwrap();
    }

    #[test]
    fn a_shorter_response_header_timeout_keeps_its_reason() {
        let (url, server) = loopback(|mut stream| {
            request(&mut stream);
            let _ = stream.read(&mut [0]);
        });
        let client = test_agent(Duration::from_millis(500), Duration::from_millis(50), None);
        assert!(matches!(
            client.get(&url).call(),
            Err(Error::Timeout(Timeout::RecvResponse))
        ));
        server.join().unwrap();
    }

    #[test]
    fn https_still_requires_a_tls_connection() {
        let (url, server) = loopback(|mut stream| {
            let mut record = [0; 5];
            stream.read_exact(&mut record).unwrap();
            assert_eq!(record[0], 22);
            assert_eq!(record[1], 3);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        });
        let client = test_agent(Duration::from_millis(500), Duration::from_secs(2), None);
        assert!(
            client
                .get(url.replacen("http://", "https://", 1))
                .call()
                .is_err()
        );
        server.join().unwrap();
    }

    #[test]
    fn the_default_connect_proxy_and_connection_pool_are_preserved() {
        let (proxy, server) = loopback(|mut stream| {
            assert!(request(&mut stream).starts_with("CONNECT 192.0.2.1:80 HTTP/1.1\r\n"));
            stream
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .unwrap();
            for _ in 0..2 {
                assert!(request(&mut stream).starts_with("GET /component HTTP/1.1\r\n"));
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                    .unwrap();
            }
        });
        let client = test_agent(
            Duration::from_millis(500),
            Duration::from_secs(2),
            Some(ureq::Proxy::new(&proxy).unwrap()),
        );
        for _ in 0..2 {
            assert_eq!(
                client
                    .get("http://192.0.2.1/component")
                    .call()
                    .unwrap()
                    .body_mut()
                    .read_to_vec()
                    .unwrap(),
                b"ok"
            );
        }
        server.join().unwrap();
    }

    #[derive(Debug)]
    struct ProbeTransport {
        buffers: LazyBuffers,
        calls: Arc<Mutex<Vec<(usize, time::Duration, Timeout)>>>,
    }

    impl Transport for ProbeTransport {
        fn buffers(&mut self) -> &mut dyn Buffers {
            &mut self.buffers
        }
        fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), Error> {
            self.calls
                .lock()
                .unwrap()
                .push((amount, timeout.after, timeout.reason));
            Ok(())
        }
        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, Error> {
            self.calls
                .lock()
                .unwrap()
                .push((0, timeout.after, timeout.reason));
            Ok(true)
        }
        fn is_open(&mut self) -> bool {
            false
        }
        fn is_tls(&self) -> bool {
            true
        }
    }

    #[test]
    fn only_reads_are_capped_and_tls_buffers_and_pool_health_are_delegated() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut transport = IdleTransport {
            inner: Box::new(ProbeTransport {
                buffers: LazyBuffers::new(16, 16),
                calls: calls.clone(),
            }),
            idle: Duration::from_secs(120),
        };
        assert!(transport.is_tls());
        assert!(!transport.is_open());
        transport.buffers().output()[0] = 42;
        assert_eq!(transport.inner.buffers().output()[0], 42);
        transport
            .transmit_output(
                1,
                NextTimeout {
                    after: time::Duration::from_secs(200),
                    reason: Timeout::SendBody,
                },
            )
            .unwrap();
        for (after, reason) in [
            (time::Duration::NotHappening, Timeout::Global),
            (time::Duration::from_secs(200), Timeout::PerCall),
            (time::Duration::from_secs(120), Timeout::RecvResponse),
            (time::Duration::from_secs(15), Timeout::Connect),
            (time::Duration::from_secs(0), Timeout::Global),
        ] {
            assert!(
                transport
                    .await_input(NextTimeout { after, reason })
                    .unwrap()
            );
        }
        assert_eq!(
            *calls.lock().unwrap(),
            vec![
                (1, time::Duration::from_secs(200), Timeout::SendBody),
                (0, time::Duration::from_secs(120), Timeout::RecvBody),
                (0, time::Duration::from_secs(120), Timeout::RecvBody),
                (0, time::Duration::from_secs(120), Timeout::RecvResponse),
                (0, time::Duration::from_secs(15), Timeout::Connect),
                (0, time::Duration::from_secs(0), Timeout::Global),
            ]
        );
    }
}
