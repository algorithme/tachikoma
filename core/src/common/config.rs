use std::{
    env, io,
    net::{Ipv4Addr, SocketAddr},
};

pub(crate) struct Config {
    port: u16,
}

impl Config {
    pub(crate) fn from_env() -> io::Result<Self> {
        let value = match env::var("TACHIKOMA_PORT") {
            Ok(value) => Some(value),
            Err(env::VarError::NotPresent) => None,
            Err(env::VarError::NotUnicode(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "TACHIKOMA_PORT must be valid Unicode containing a port from 1 to 65535",
                ));
            }
        };

        Ok(Self {
            port: parse_port(value.as_deref())?,
        })
    }

    pub(crate) fn bind_addr(&self) -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, self.port))
    }
}

fn parse_port(value: Option<&str>) -> io::Result<u16> {
    let Some(value) = value else {
        return Ok(3000);
    };

    value
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "TACHIKOMA_PORT must be a port from 1 to 65535",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::parse_port;
    use std::io::ErrorKind;

    #[test]
    fn port_defaults_and_validation() {
        let cases = [
            (None, Ok(3000)),
            (Some("1"), Ok(1)),
            (Some("3000"), Ok(3000)),
            (Some("65535"), Ok(65535)),
            (Some(""), Err(ErrorKind::InvalidInput)),
            (Some("abc"), Err(ErrorKind::InvalidInput)),
            (Some("-1"), Err(ErrorKind::InvalidInput)),
            (Some("0"), Err(ErrorKind::InvalidInput)),
            (Some("65536"), Err(ErrorKind::InvalidInput)),
            (Some(" 3000 "), Err(ErrorKind::InvalidInput)),
        ];

        for (input, expected) in cases {
            assert_eq!(
                parse_port(input).map_err(|error| error.kind()),
                expected,
                "TACHIKOMA_PORT={input:?}",
            );
        }
    }
}
