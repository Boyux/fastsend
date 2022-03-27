use crate::Serialer;
use itertools::Itertools;
use md5::Digest;
use rand::{distributions::Standard, prelude::*};
use rand_chacha::{rand_core::block::BlockRng, ChaCha20Core};
use sha1::{Digest as Sha1Digest, Sha1};
use std::cell::RefCell;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

/// ## UUID
///
/// ~~使用 MD5 构造的版本 3、变体 1 的 UUID，虽然在密码学上 MD5 非常不安全， 但仅用作构造唯一 UUID，
/// MD5 摘要算法已经足够，并不需要使用版本 5 的 SHA1 摘要算法。~~
///
/// 尽管如此，还是简单地实现了 V4 和 V5 版本的 UUID，作为备选。V4 版本的 UUID 采用了密码学安全的 chacha20
/// 随机数生成算法；V5 版本的 UUID 采用 sha-1 作为哈希算法（这些都是符合 UUID 版本标准的）。其中，由于 sha-1
/// 生成的摘要信息超过 128-bit，因此仅截取前 128 bits 作为 UUID 值。
#[derive(Debug)]
pub struct UUIDSerialer {
    data: Vec<u8>,

    /// 版本号，仅支持 V3、V4、V5
    version: Version,
}

impl UUIDSerialer {
    pub fn new_v3() -> UUIDSerialer {
        UUIDSerialer {
            data: Vec::with_capacity(32),
            version: Version::V3,
        }
    }

    pub fn new_v4() -> UUIDSerialer {
        UUIDSerialer {
            data: Vec::with_capacity(0),
            version: Version::V4,
        }
    }

    pub fn new_v5() -> UUIDSerialer {
        UUIDSerialer {
            data: Vec::with_capacity(64),
            version: Version::V5,
        }
    }
}

impl Serialer for UUIDSerialer {
    type Output = String;

    type Error = Infallible;

    fn build(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>> {
        #[inline]
        fn to_uuid(bytes: impl Iterator<Item = u8>, version: Version) -> String {
            bytes
                .take(16)
                .enumerate()
                .format_with("", |(index, byte), f| {
                    if index == 6 {
                        // UUID 第三部分的第 1 位字符代表版本号，即字节的前 4 位 bit 通过字符形式表示版本，
                        // 由于是固定值，该字节只保留后四位 bit
                        f(&format_args!("-{:x}{:x}", version, byte & 0x0f))
                    } else if index == 8 {
                        // UUID 第四部分的第一个字节前 2 位 bit，即 '10' 代表变体 1，为固定值，因此需要将
                        // 该字节前 2 位 bit 置为 '10'，先清除前 2 位 bit 的值，再赋值 '10'
                        f(&format_args!("-{:02x}", byte & 0x3f | 0x80))
                    } else if [4, 10].contains(&index) {
                        f(&format_args!("-{:02x}", byte))
                    } else {
                        f(&format_args!("{:02x}", byte))
                    }
                })
                .to_string()
        }

        let uuid = match self.version {
            Version::V3 => {
                let Digest(a) = md5::compute(self.data);
                to_uuid(a.iter().copied(), self.version)
            }
            Version::V4 => {
                thread_local! {
                    static RNG: Rc<RefCell<BlockRng<ChaCha20Core>>> = Rc::new(RefCell::new(BlockRng::new(ChaCha20Core::from_entropy())));
                }

                assert!(self.data.is_empty());
                let rng = RNG.with(|rng| rng.clone());
                let mut mut_rng = rng.borrow_mut();
                let bytes_iter = (&mut *mut_rng).sample_iter(Standard);
                to_uuid(bytes_iter, self.version)
            }
            Version::V5 => {
                let mut sha = Sha1::new();
                sha.update(&self.data);
                to_uuid(sha.finalize().iter().copied(), self.version)
            }
        };

        Box::pin(async move { Ok(uuid) })
    }

    fn feed(&mut self, data: &[u8]) {
        // V4 版本的 UUID 采用密码学安全的随机数生成，因此不需要提供任何额外数据
        if self.version != Version::V4 {
            self.data.extend_from_slice(data);
        }
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Version {
    V3 = 3,
    V4 = 4,
    V5 = 5,
}

impl fmt::LowerHex for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::LowerHex::fmt(&((*self) as i32), f)
    }
}
