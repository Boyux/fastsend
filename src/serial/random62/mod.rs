use crate::Serialer;
use rand::{distributions::Alphanumeric, prelude::*};
use rand_chacha::{rand_core::block::BlockRng, ChaCha20Core};
use std::convert::{Infallible, TryInto};
use std::future::Future;
use std::iter;
use std::pin::Pin;

#[derive(Debug)]
pub struct Random62Serialer {
    seed: Vec<u8>,
}

/// ## Random-ID-62
///
/// 使用密码学安全的随机数生成基于 62 个字符的 35 字节的随机序列，用作 ID。
impl Random62Serialer {
    pub fn new() -> Random62Serialer {
        Random62Serialer {
            seed: Vec::with_capacity(32),
        }
    }
}

impl Serialer for Random62Serialer {
    type Output = String;

    type Error = Infallible;

    fn build(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>> {
        let seed: [u8; 32] = self
            .seed
            .iter()
            .copied()
            .chain(iter::repeat(0))
            .take(32)
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        let output = BlockRng::new(ChaCha20Core::from_seed(seed))
            .sample_iter(Alphanumeric)
            .take(35)
            .map(char::from)
            .collect();

        Box::pin(async move { Ok(output) })
    }

    fn feed(&mut self, data: &[u8]) {
        self.seed.extend_from_slice(data);
    }
}
