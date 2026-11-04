use std::io::Write;

/// How a request body goes to a worker or an events URL. Its signature covers
/// the bytes as sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compress {
    Gzip,
}

/// `body` as it is sent, and its `Content-Encoding`.
pub fn encode(
    compress: Option<Compress>,
    body: Vec<u8>,
) -> std::io::Result<(Vec<u8>, Option<&'static str>)> {
    match compress {
        None => Ok((body, None)),
        Some(Compress::Gzip) => {
            let mut gz = flate2::write::GzEncoder::new(
                Vec::with_capacity(body.len() / 4),
                flate2::Compression::default(),
            );
            gz.write_all(&body)?;
            Ok((gz.finish()?, Some("gzip")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn gzip_round_trips_and_none_is_untouched() {
        let body = br#"{"items":["a","a","a","a","a","a","a","a"]}"#.repeat(100);
        let (sent, encoding) = encode(Some(Compress::Gzip), body.clone()).unwrap();
        assert_eq!(encoding, Some("gzip"));
        assert!(sent.len() < body.len() / 5);
        let mut back = Vec::new();
        flate2::read::GzDecoder::new(&sent[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, body);
        assert_eq!(encode(None, body.clone()).unwrap(), (body, None));
    }
}
