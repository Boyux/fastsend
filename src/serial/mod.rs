use chrono::Local;
use crossbeam::utils::Backoff;
// 使用 `futures_locks` 的读写锁来提供对（`Serialer`）异步任务的支持
use futures::executor;
use futures_locks::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use lazy_static::lazy_static;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::fmt::{Display, Write};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::thread;

/// `Serial` 类似于 `Hash` trait，消耗自身，将有关数据喂给 `Serialer`。
pub trait Serial {
    fn serial<S: Serialer>(self, serialer: &mut S);
}

/// `Serialer` trait 代表了用于生成序列号的类型，其定义方式类似于标准库中的 `Hasher` trait，
/// 但不同于 `Hasher`，在完成对序列号的构建的 `build` 方法会消耗 `Serialer` 本身（而 `Hasher`
/// 的 `finish` 方法则仅使用了 `&self`）。
///
/// 而与 `Hasher` 另一个不同点在于，对于同一个 `Hasher`，如果写入相同的数据，其产生的哈希值应是
/// 相同的；但对于 `Serialer` 来说却有所不同，根据对 `Serialer` 的定义：
///
/// # 定义
///
/// 对于每一个独立的 `Serialer` 实例，不论其是否被 `feed` 了相同的数据，最终使用 `build` 生成
/// 的序列号均应具有唯一性，换句话说，`Serialer` 应保证产生的每一个序列号均不与相同 `Serialer`
/// 产生的序列号重复（N 个 `Serialer` 生成的序列号数量应被保证为 N）。
pub trait Serialer {
    /// `Output` 是代表序列号的类型，其应实现 `Display` trait，这也意味着序列号能通过 `ToString`
    /// trait 转化为 `String` 类型。
    type Output: Display;

    /// `Error` 是序列号构建过程中，当构建失败时返回的错误类型，类似于 `serde` 中的序列化错误，如果
    /// 序列号构建过程不会返回失败，建议使用 `Infallible` 类型作为错误类型。
    type Error;

    /// `build` 方法类似于 `Hasher::finish` 方法，消耗自身构建出序列号，值得注意的是，在许多场景下，
    /// 构建序列号的方式会依赖外部系统，因此在设计 `build` 的 API 时，考虑到调用外部系统往往需要使用
    /// `Future` 来支持异步任务，在返回值的设计上选择了 `Pin<Box<dyn Future>>` 来提供对 async/await
    /// 的支持（这也是在当前 Rust 版本下，语法层面不支持 `async-trait` 的一种妥协方案）。
    ///
    /// 由于异步系统的外部依赖性，通常而言并不能百分百保证序列号构建的正确性，即错误是无法避免的，因此返回
    /// 结果应是一个 `Result` 类型，用于处理序列号构建失败的情况。
    ///
    /// 需要额外注意的是，返回的 `Future` 需要满足 `Send` + `'static` 的约束，这是为了适配多数异步
    /// `Runtime` 中多线程异步任务执行器对 `Future` 的约束。
    fn build(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>>;

    /// `feed` 用于向 `Serialer` 提供用于生成序列号的必要信息，类似于 `Hasher` trait 中的 `write` 方法。
    fn feed(&mut self, data: &[u8]);

    /// `oneshot` 用于直接根据提供的数据 `data` 构建序列号，是 'feed+build' 的快捷方式，省去了需要先调用
    /// `feed` 再调用 `build` 的麻烦，能更好地进行链式调用（当你已有一个 'Serialer' 实例时，可以直接调用
    /// `serialer.oneshot.await` 完成序列号构建）。
    fn oneshot<S: Serial>(
        mut self,
        data: S,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>>
    where
        Self: Sized,
    {
        data.serial(&mut self);
        self.build()
    }
}

/// `TimeSerialer` 是基于时间的序列号生成器，该序列号由纯数字组成，其特点在于可以从序列号一眼看出生成的 时间节
/// 点（精确到秒）。
///
/// 该序列号生成器采用了类似全局变量的方式来解决序列号冲突的问题，其构造了一个全局 slot 用于存储短时间内生成的序列
/// 号，该 slot 的实现方式为 HashMap，并使用了全局读写锁，新产生的序列号将先在该 slot 中查询是否已被创建过（是否
/// 冲突），只有在非冲突的场合下才会完成序列号生成。
///
/// `TimeSerialer` 具有对全局 slot 的定时清理功能，当 slot 存储的序列号超过一定阈值时会触发清理任务，将在额外的
/// 线程完成对 slot 的清理，最早时间节点创建的序列号将从 slot 中丢弃，因为它们（指这些被丢弃的序列号）已经被证实不
/// 会再次出现。
#[derive(Debug)]
pub struct TimeSerialer(Vec<u8>);

impl TimeSerialer {
    const GLOBAL_SLOT_SIZE: usize = 9999;

    pub fn new() -> Self {
        TimeSerialer(Vec::with_capacity(8))
    }
}

impl Default for TimeSerialer {
    fn default() -> Self {
        Self::new()
    }
}

impl Serialer for TimeSerialer {
    type Output = String;

    type Error = Infallible;

    fn build(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>> {
        lazy_static! {
            /// 全局 `SLOT` 容器，用于存储在一定时间段内生成的序列号，用于判断是否重复。
            static ref SLOT: RwLock<HashMap<String, i64>> = {
                // 使用 `Cursor` 来保证在程序短时间内多次重启时，生成的序列号能保证唯一性。
                #[allow(unused)]
                #[cfg(feature = "pause_on_start")]
                let cursor = crate::Cursor::new().next();

                RwLock::new(HashMap::with_capacity(TimeSerialer::GLOBAL_SLOT_SIZE))
            };
        }

        let backoff = Backoff::new();

        Box::pin(async move {
            loop {
                // 时间不仅要用来构建序列号，还需要用来定位序列号生成的时间，用于定时清空全局 HashMap 的元素
                let now = Local::now();

                // 使用填充法构建序列号
                let serial = {
                    // 预留 14+3+4=21 的空间用于填充序列号，`TimeSerialer` 生成长度为 21 的纯数字序列号。
                    // "XXXXXXXXXXXXXXXXXXXXX"
                    let mut buffer = String::with_capacity(14 + 3 + 4);

                    // 序列号的前 14 位，由精确到秒的具有人类可读性的时间序列组成，其格式类似于 '20211209113031'。
                    buffer
                        .write_fmt(format_args!("{:014}", now.format("%Y%m%d%H%M%S")))
                        .expect("error writing datetime into string buffer");

                    // 序列号的中间 3 位，由设备 ID 决定，设备 ID 源于环境变量 `FASTSEND_DEVICE_ID`，如果未提供
                    // 环境变量，则使用随机生成的 u8 整数（8-bit）值（在单设备环境下，可以更好地减少序列号碰撞）。
                    buffer
                        .write_fmt(format_args!(
                            "{:03}",
                            crate::DEVICE_ID.unwrap_or_else(rand::random)
                        ))
                        .expect("error writing first byte(u8) into string buffer");

                    // 序列号的后 5 位，由 `feed` 带来的字节序列经过哈希后对 10000 取模生成，为保证序列号尽可能短，
                    // 碰撞的情况是不可避免的，但通常而言，一秒钟内生成 9999 个序列号已经能满足大部分场景的需求。
                    let ident = {
                        // 直接构造 `DefaultHasher` 而非使用 `RandomState` 是为了确保相同的 `feed` 能产生相同
                        // 的哈希值，进而确保 `serial` 的后 4 位能保持一致。
                        let mut hasher = DefaultHasher::new();
                        self.0.hash(&mut hasher);
                        let sum = hasher.finish();
                        (sum ^ (sum >> 32)) % 10000
                    };

                    buffer
                        .write_fmt(format_args!("{:04}", ident))
                        .expect("error writing bytes(u16) into string buffer");

                    buffer
                };

                // 优先使用 `read-lock` 来判断序列号是否重复，如果重复，则在 `snooze` 后重新获取序列号，在序列号
                // 冲突的时间点内（秒），使用 `read-lock` 能在很大程度上提升性能。
                {
                    let locked_slot: RwLockReadGuard<HashMap<String, i64>> =
                        RwLock::read(&*SLOT).await;

                    if locked_slot.contains_key(&serial) {
                        backoff.snooze();
                        continue;
                    }
                }

                // 当前序列号是唯一序列号，此时需要把序列号保存到全局 `HashMap` 中用于判断唯一性，如果全局 `HashMap`
                // 容量已经超过单秒内所能产生的所有序列号（9999 个），则需要对 `HashMap` 进行清理。
                {
                    let mut locked_slot_mut: RwLockWriteGuard<HashMap<String, i64>> =
                        RwLock::write(&*SLOT).await;

                    // 双锁判断，确保在读写锁之间出现序列号冲突的情况
                    if locked_slot_mut.contains_key(&serial) {
                        backoff.snooze();
                        continue;
                    }

                    // 在将序列号保存到全局 `HashMap` 时，需要同时保存时间戳（作为 value）用于后续清理时判断该序列号
                    // 是否需要被清理。
                    locked_slot_mut.insert(serial.clone(), now.timestamp());

                    // 当 slot 的容量超过 `GLOBAL_SLOT_SIZE` 时，开始清理工作
                    if locked_slot_mut.len() > TimeSerialer::GLOBAL_SLOT_SIZE {
                        // 新起一个线程来执行清理任务，以便能快速返回生成的序列号，减少阻塞时间
                        thread::spawn(|| {
                            // 由于是在新的线程中完成对 slot 的清理，因此使用 `block_on` 方法阻塞式地执行
                            // `Future` 并不会影响全局异步任务（Runtime）的进行。
                            executor::block_on(async move {
                                // 在新的线程执行异步任务，需要重新获取 `locked_slot_mut` 来执行清理动作
                                let mut locked_slot_mut: RwLockWriteGuard<HashMap<String, i64>> =
                                    RwLock::write(&*SLOT).await;

                                // `sorted_list` 是用于判断哪个时间点前的序列号需要被清理的一个辅助工具，
                                // 通过取出 slot 中所有的时间戳构成。
                                let sorted_list = {
                                    let mut list = locked_slot_mut
                                        // 取出所有的时间戳
                                        .values()
                                        .copied()
                                        // 将时间戳去重
                                        .collect::<HashSet<i64>>()
                                        .into_iter()
                                        // 最后构造成 list
                                        .collect::<Vec<i64>>();

                                    // 对 list 进行排序，在这种无关排序稳定性的情况下，使用 `sort_unstable`
                                    // 比使用 `sort` 要快不少（来自 cargo-clippy 的指点）。
                                    list.sort_unstable();
                                    list
                                };

                                // 当且仅当 list 的元素数量大于 1 时（list 已经去重）才进行 slot 清理，当
                                // list 中的元素数量小于等于 1 时，进行清理会将 slot 中的所有元素都删除，
                                // 这会导致重复判定机制失效。
                                if sorted_list.len() > 1 {
                                    // `mid` 代表 `HashMap` 中所有时间戳的中位数，它应至少是 `sorted_list`
                                    // 中的第二个元素，所有小于 `mid` 时间戳的序列号均应被删除，因为当前时间已
                                    // 经大于该时间戳，新生成的序列号永远不会与 `mid` 时间戳之前生成的序列号重
                                    // 复。
                                    //
                                    // （其实从原理上来讲，`mid` 完全可以使用 `sorted_list` 的最后一个元素，
                                    // 但此处使用 `sorted_list` 长度的一半作为索引获取 `mid`，是处于性能考
                                    // 虑，一次性删除过多的元素会导致长时间的阻塞，因此此处试图减少删除的元素来
                                    // 降低锁阻塞的时间。）
                                    let mid = sorted_list[sorted_list.len() / 2];

                                    // 将小于 `mid` 时间戳的序列号从 slot 中删除，并用新生成的 `HashMap`
                                    // 代替原来的 slot
                                    *locked_slot_mut = locked_slot_mut
                                        .iter()
                                        // `filter` 出大于等于 `mid` 的序列号留下，其余小于 `mid` 的序列号
                                        // 通通丢弃
                                        .filter(|(_, t)| **t >= mid)
                                        .map(|(s, t)| (s.clone(), *t))
                                        .collect();
                                }
                            })
                        });
                    }
                }

                return Ok(serial);
            }
        })
    }

    fn feed(&mut self, data: &[u8]) {
        self.0.extend_from_slice(data);
    }
}

fn to_string_radix(mut n: usize, radix: usize, size: usize, digit_first: bool) -> String {
    assert!(radix >= 2 && radix <= 36);

    let bytes_table: [char; 36] = if digit_first {
        [
            '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'A', 'B', 'C', 'D', 'E', 'F', 'G',
            'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X',
            'Y', 'Z',
        ]
    } else {
        [
            'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M', 'N', 'O', 'P', 'Q',
            'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z', '0', '1', '2', '3', '4', '5', '6', '7',
            '8', '9',
        ]
    };

    let mut buf = String::with_capacity(2);

    loop {
        let m = n % radix;
        n = n / radix;
        buf.push(bytes_table[m]);
        if n == 0 {
            break;
        }
    }

    use std::cmp;
    (0..(size - cmp::min(size, buf.len()))).for_each(|_| buf.push(bytes_table[0]));

    unsafe {
        buf.as_mut_vec().reverse();
    }

    buf
}

#[cfg(feature = "ticket")]
pub mod ticket;

#[cfg(feature = "uuid")]
pub mod uuid;

#[cfg(feature = "auto_increment")]
pub mod auto_increment;

#[cfg(feature = "random62")]
pub mod random62;
