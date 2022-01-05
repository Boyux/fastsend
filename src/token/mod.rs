use crate::{Block, BlockFrame, ConstructBlock, Cursor, Serial, Serialer, ID};
use std::mem::{self, MaybeUninit};

/// `Token` 是一个完全独立的标记，通常用于表示某个完全独立的事物，其由两个部分组成：
/// `Cursor` 和 `Ident`，分别代表了 `Token` 生成的时间和该时间下代表事物独立性
/// 的一些要素。
///
/// # 注意
///
/// `Cursor` 虽然有时间含义，但由于其生成方式为计算时间间隔，因此其并不能代表 `Token`
/// 生成的具体时间，只能说该时间仅用于区分不同 `Token` 的生成时间。
///
/// `Ident` 代表了一系列可组成一个独立标识的各项元素的组合，目前对 `Ident` 的实现为
/// 依赖设备号、线程 ID、进程 ID 以及通过发号机创建的编号，`Ident` 是有可能重复的，只
/// 有将 `Ident` 和 `Cursor` 组合才能完成一个独立的 `Token`。
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct Token {
    cursor: Cursor,
    ident: Ident,
}

impl Token {
    fn new(cursor: Cursor, ident: Ident) -> Self {
        Token { cursor, ident }
    }
}

impl ID for Token {
    fn id(self) -> u64 {
        let mut bytes = [0; 8];

        #[derive(Debug)]
        struct Guard<'a> {
            index: usize,
            slice: &'a mut [u8],
        }

        impl<'a> Guard<'a> {
            fn set(&mut self, next: &u8) {
                if self.index < self.slice.len() {
                    self.slice[self.index] = *next;
                    self.index += 1;
                }
            }
        }

        let mut guard = Guard {
            index: 0,
            slice: &mut bytes,
        };

        self.cursor
            .into_inner()
            .to_be_bytes()
            .iter()
            .for_each(|next| guard.set(next));

        self.ident
            .construct()
            .to_be_bytes()
            .iter()
            .for_each(|next| guard.set(next));

        u64::from_be_bytes(bytes)
    }
}

impl Serial for Token {
    fn serial<S: Serialer>(self, serialer: &mut S) {
        // 将 `Cursor` 转化为 u32，再根据大端序转化为数组 [u8; 4] 并 feed 给 `serialer`
        serialer.feed(&self.cursor.into_inner().to_be_bytes());

        // 将 `Ident` 中的四个字段 `a`/`b`/`c`/`d` 组成数组 feed 给 `serialer`
        serialer.feed(&[self.ident.a, self.ident.b, self.ident.c, self.ident.d]);
    }
}

impl ConstructBlock for Token {
    fn construct_block(n: usize, cursor: Cursor) -> Block<Self> {
        debug_assert!(n <= BlockFrame::<Self>::QUEUE_SIZE);

        let n = n as u16;
        let size = Block::<Self>::SIZE as u16;

        // 使用 Rust 官方推荐的对数组进行 "逐个元素初始化"，使用了 `MaybeUninit`。
        // （具体说明请查看 `MaybeUninit` 的文档）
        let array = unsafe {
            // Create an uninitialized array of `MaybeUninit`. The `assume_init` is
            // safe because the type we are claiming to have initialized here is a
            // bunch of `MaybeUninit`s, which do not require initialization.
            let mut array: [MaybeUninit<Token>; Block::<Self>::SIZE] =
                MaybeUninit::uninit().assume_init();

            // Dropping a `MaybeUninit` does nothing. Thus using raw pointer
            // assignment instead of `ptr::write` does not cause the old
            // uninitialized value to be dropped. Also if there is a panic during
            // this loop, we have a memory leak, but there is no memory safety
            // issue.
            for (i, elem) in array.iter_mut().enumerate() {
                let offset = i as u16;
                *elem = MaybeUninit::new(Token::new(cursor, Ident::new(n * size + offset)));
            }

            // Everything is initialized. Transmute the array to the
            // initialized type.
            mem::transmute::<_, [Token; Block::<Self>::SIZE]>(array)
        };

        Block::new(array)
    }
}

/// `Ident` 表示在某一特定时间节点下的一系列具有独立性的要素，目前的实现方式中包含四个部分，见字段说明。
/// （`c` 和 `d` 的实现详见于 `cd` 函数的注释）
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
struct Ident {
    /// `a` 和 `b` 是一组数据，是从发号机中获取的一个 u16 数字拆分成两个字节，这里的发号机特指 `BlockFrame`
    /// 及 `BlockFrame` 所产生的 `Block`。
    a: u8,
    b: u8,

    /// `c` 的实现有两种，如果提供设备号，则使用设备号作为 `c`，如果未提供设备号，则截取进程 ID 的后 8 位。
    c: u8,

    /// `d` 的实现依赖于线程 ID，在通常情况下无法从 Rust 标准库中获取 u64 类型的线程 ID，因此使用一些特殊但
    /// 又常规的方式获取线程 ID 的最后 8 位。
    d: u8,
}

impl Ident {
    fn new(n: u16) -> Self {
        let [a, b] = n.to_be_bytes();
        let (c, d) = cd();
        Ident { a, b, c, d }
    }

    fn construct(self) -> u32 {
        u32::from_be_bytes([self.a, self.b, self.c, self.d])
    }
}

use lazy_static::lazy_static;
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::{process, thread};

/// `cd` 用于通过系统环境获取两个 u8 数值用于构建 `Ident`，通常而言 `cd` 代表着设备信息，用于
/// 区分不同的设备，而环境信息则选择了进程和线程 id，并通过部分截取来构建 `cd`。
fn cd() -> (u8, u8) {
    // 对于 c 值，如果有从环境变量提供的 "FASTSEND_DEVICE_ID" 值，则取该值，如果没有
    // 则取进程 id 后八位作为 c 值以增加随机性。
    let c = if let Some(device_id) = *crate::DEVICE_ID {
        device_id
    } else {
        process::id() as u8
    };

    // 使用 thread_id 后八位（u8 大小）作为 `d` 的值，增加整体 `cd` 随机性
    let d = {
        lazy_static! {
            // 如同标准库中对 Hasher 的描述，使用 `RandomState` 来防止哈希洪水攻击
            static ref RANDOMSTATE: RandomState = RandomState::new();
        }

        // 通常而言，在 Rust 中是无法直接获取数值类型的 thread_id 的，标准库里使用
        // `NonZeroU64` 作为 thread_id，但并未暴露这个 64 位整数的具体数值，仅允许
        // 对 thread_id 进行比较、判断和哈希操作，因此为了构造出能【部分】代替 thread_id
        // 的数值，我们使用哈希的方式，将标准库的 thread_id 哈希后，取最后 8 位作为
        // 代表 thread_id 的值，从期望上讲，这样做与直接取 thread_id 后 8 位数值
        // 在理论上是一样的，毕竟 thread 不可能只有 256 个，数值碰撞是无法避免的。
        let mut hasher = BuildHasher::build_hasher(&*RANDOMSTATE);
        thread::current().id().hash(&mut hasher);
        let sum = hasher.finish();

        // 使用哈希值的前 32 位与后 32 位做异或操作，并取最后八位作为代表 thread_id 的值，
        // 目的为增加 thread_id 的随机性，减少碰撞（虽然实际上 u8 大小的数值（256）本身碰
        // 撞率就 非常高）
        (sum ^ (sum >> 32)) as u8
    };

    (c, d)
}
