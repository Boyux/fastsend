use crate::{Serialer, RV};
use lazy_static::lazy_static;
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicI64, AtomicU8, Ordering},
    Mutex,
};

/// 代表 STATE 尚未初始化的常量，获取到该值表明 STATE 尚未初始化完毕
pub const UNINITIALIZED: i64 = -1;

/// 代表增长失败的常量，获取到该值表明 STATE 在自增过程中出现了错误导致失败
pub const FAILED: i64 = i64::MIN;

lazy_static! {
    static ref COUNTER: AtomicU8 = AtomicU8::new(*RV);
}

/// 由于自增序列需要保存当前自增的状态，因此使用 `IncrState` 来保存各类状态和值，
/// 其相当于一个 Builder，类似于标准库中的 `BuildHasher` trait；由于 `Serialer`
/// 在构建时会消耗自身所有权，因此需要 'Builder' 来不断生成新的 `Serialer`
pub struct IncrState<AI: AutoIncrement> {
    /// 代表生成用于构建序列号 `ident` 的类型，其实现的 `AutoIncrement` trait
    /// 让 `IncrState` 具有自增的能力，由于通常而言 `IncrState` 作为全局变量存在，
    /// 因此 `AutoIncrement` 未设计成异步 trait
    engine: Mutex<AI>,

    /// 可选项：序列号前缀
    prefix: Option<Box<str>>,

    /// 序列号后缀，由程序自身全局的原子计数器维持，每次左旋 1 bit 后再 +1，全局计数器的来源为
    /// 随机数 `RV`，该值由编译期环境变量获取，或运行时随机生成；`suffix` 的意义在于提升序列号
    /// 猜测成本，使序列号单调递增却又不容易看出规律
    suffix: &'static AtomicU8,

    /// 代表生成序列号时，用于前向填充的 '0' 的数量
    padding: Option<usize>,

    /// 代表最后一次生成的 `ident` 值
    last: AtomicI64,
}

impl<AI: AutoIncrement> IncrState<AI> {
    pub fn builder() -> IncrStateBuilder {
        IncrStateBuilder::new()
    }

    pub fn incr(&self) -> IncrSerialer {
        assert!(!self.engine.is_poisoned());
        IncrSerialer {
            ident: {
                let mut engine = self.engine.lock().unwrap();
                let old = self.last.load(Ordering::SeqCst);
                let new = engine.incr(old);
                if new == UNINITIALIZED {
                    UNINITIALIZED
                } else {
                    if new <= old {
                        FAILED
                    } else {
                        self.last.store(new, Ordering::SeqCst);
                        new
                    }
                }
            },
            prefix: self.prefix.as_deref(),
            suffix: {
                let suffix;
                loop {
                    let old = self.suffix.load(Ordering::SeqCst);
                    let new = old.rotate_left(1).wrapping_add(1);
                    if self
                        .suffix
                        .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                    {
                        suffix = new;
                        break;
                    }
                }
                suffix
            },
            padding: self.padding.unwrap_or_default(),
        }
    }
}

pub struct IncrSerialer<'a> {
    ident: i64,
    prefix: Option<&'a str>,
    suffix: u8,
    padding: usize,
}

impl<'a> Serialer for IncrSerialer<'a> {
    type Output = String;

    type Error = i64;

    fn build(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>> {
        let output = format!(
            "{prefix}{ident:0padding$}{suffix:02}",
            ident = self.ident,
            padding = self.padding,
            prefix = self.prefix.unwrap_or_default(),
            suffix = self.suffix % 100,
        );

        Box::pin(async move {
            match self.ident {
                UNINITIALIZED => Err(UNINITIALIZED),
                FAILED => Err(FAILED),
                _ => Ok(output),
            }
        })
    }

    fn feed(&mut self, _: &[u8]) {}
}

pub trait AutoIncrement {
    fn incr(&mut self, current: i64) -> i64;
}

impl<F: FnMut(i64) -> i64> AutoIncrement for F {
    fn incr(&mut self, current: i64) -> i64 {
        (self)(current)
    }
}

#[cfg(debug_assertions)]
#[allow(dead_code)]
mod static_check {
    use super::*;

    lazy_static! {
        static ref STATE: IncrState<fn(i64) -> i64> = IncrState {
            engine: Mutex::new(next),
            prefix: None,
            suffix: &COUNTER,
            padding: None,
            last: AtomicI64::new(UNINITIALIZED),
        };
    }

    fn next(_: i64) -> i64 {
        unreachable!()
    }
}

pub struct IncrStateBuilder {
    start: Option<i64>,
    prefix: Option<Box<str>>,
    padding: Option<usize>,
}

impl IncrStateBuilder {
    pub fn new() -> IncrStateBuilder {
        IncrStateBuilder {
            start: None,
            prefix: None,
            padding: None,
        }
    }

    pub fn with_start(mut self, value: i64) -> IncrStateBuilder {
        assert!(value >= 0);
        self.start = Some(value);
        self
    }

    pub fn with_prefix(mut self, prefix: &str) -> IncrStateBuilder {
        self.prefix = Some(prefix.to_owned().into_boxed_str());
        self
    }

    pub fn with_padding(mut self, padding: usize) -> IncrStateBuilder {
        if self.prefix.is_some() {
            self.padding = Some(padding);
        }
        self
    }

    pub fn build<AI: AutoIncrement>(self, engine: AI) -> IncrState<AI> {
        IncrState {
            engine: Mutex::new(engine),
            prefix: self.prefix,
            suffix: &COUNTER,
            padding: self.padding,
            last: AtomicI64::new(self.start.unwrap_or(UNINITIALIZED)),
        }
    }
}
