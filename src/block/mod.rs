use crossbeam::atomic::AtomicCell;
use crossbeam::queue::ArrayQueue;
use crossbeam::utils::Backoff;
use lazy_static::lazy_static;
use rand::seq::SliceRandom;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Instant, SystemTime};

/// # BlockFrame 的设计理念
///
/// 对于特定场景，在事先知道需要构造大量元素 T 的场合，为了避免每次单次生成所花费的边际成本过高，因此使用一定的算法
/// 事先生成特定数量的元素 T，并划分成不同的 `Block` 使用，每个 `Block` 被设计成被 Sent 到不同线程以供使用，这
/// 种设计解决了多线程竞争同一个消费队列产生的锁竞争消耗。
///
/// 一个 `Block` 在目前的设计中被定义为包含 `Block::SIZE=8` 个元素的集合，并且实现了 `Iterator` trait，预示
/// 着其是一个可以生成总计 `Block::SIZE` 个数元素 T 的生成器。`Block` 通常会被放置在 `thread_local` 中使用，
/// 以避免线程间的互相竞争。`Block` 将会以 `&mut Block` 的形态出现，因此其在理论上应该是只 `Send` 不 `Sync` 的。
#[derive(Debug)]
pub struct BlockFrame<T> {
    /// `cursor` 标记了当前 `BlockFrame` 所处的时间节点，其作用在于当 `queue` 队列内容不足需要进行补充时，使用
    /// `Cursor` 来正确进行时间线延后操作，即队列补充的时间线不应与当前时间线重合，避免冲突（通过调用
    /// `Cursor::next` 方法）。
    cursor: Arc<AtomicCell<Cursor>>,

    /// `fresh` 是包含所有新产生 `Block` 的队列，队列大小为 cap=QUEUE_SIZE， 在初始化及补充完成的场合，
    /// `fresh` 队列应包含全部 `Block`。
    queue: Arc<ArrayQueue<Block<T>>>,

    /// `state` 代表当前 `supply` 的执行进度，false 代表无正在执行的 `supply` 线程，true 代表当前有正在
    /// 执行的 `supply` 线程。
    state: Arc<AtomicCell<bool>>,
}

impl<T> Default for BlockFrame<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> BlockFrame<T> {
    /// `ELEMENT_CAP` 表示一个 `BlockFrame` 在一个 `Cursor` 下所能产生的所有元素的数量，该数量指 T 的
    /// 数量而非 `Block` 的数量。
    pub(crate) const ELEMENT_CAP: usize = u16::MAX as usize + 1;

    /// `QUEUE_SIZE` 指 `queue` 队列中总计的 `Block` 的数量。
    pub(crate) const QUEUE_SIZE: usize = BlockFrame::<T>::ELEMENT_CAP / Block::<T>::SIZE;

    pub fn new() -> Self {
        #[allow(unused)]
        let cursor = Cursor::new();

        // 使用 `next` 方法完成对 `Cursor` 的初始化，目的在于保证所获得的 `Cursor` 在但应用单进程场景下
        // 是唯一的，即每台机器只有一个 `fastsend` 进程，每个 `fastsend` 进程只包含一个 `BlockFrame`
        // 实例（一般以全局变量的形式存在），此举是应对短时间内（1 秒内）程序退出又重启时，可能出现的 `Cursor`
        // 冲突的情况。
        #[cfg(feature = "pause_on_start")]
        let cursor = cursor.next();

        BlockFrame {
            cursor: Arc::new(AtomicCell::new(cursor)),
            queue: Arc::new(ArrayQueue::new(Self::QUEUE_SIZE)),
            state: Arc::new(AtomicCell::new(false)),
        }
    }
}

impl<T: ConstructBlock> BlockFrame<T> {
    pub fn next_block(&self) -> Pin<Box<dyn Future<Output = Block<T>> + Send + 'static>>
    where
        T: Send + 'static,
    {
        Box::pin(BlockFuture {
            queue: Arc::clone(&self.queue),
            supply: Arc::new({
                let cursor = Arc::clone(&self.cursor);
                let queue = Arc::clone(&self.queue);
                let state = Arc::clone(&self.state);

                // `supply` 补充程序，首先通过 `Cursor::next` 方法确保补充的 `Block` 滞后于当前的 `Cursor`，
                // 这一步的目的是保证补充的 `Block` 在进行后续操作时，不与之前的 `Block` 产生时间线和数值上的冲突，
                // 即即使 `Block` 的内容与先前的 `Block` 相同，但由于已经经过 `Cursor::next` 拉长时间间隔，新
                // `Block` 是处在新的时间线上（时间线间隔为秒），所以并不会造成冲突。
                // （时间线与数值冲突指在同一时间线（秒）上，使用了相同的数值，产生冲突）
                move |mut waker| {
                    // 同一时间仅需要一个队列补充任务，通过 CAS 来确保唯一性
                    if state.compare_exchange(false, true).is_ok() {
                        let mut prev;
                        let mut next;

                        // 通过 CAS 操作将旧 `Cursor` 置换为 `next`，确保 `next` 游标一定滞后于 `prev`，
                        loop {
                            prev = cursor.load();
                            next = prev.next();
                            if cursor.compare_exchange(prev, next).is_ok() {
                                break;
                            }
                        }

                        // `ConstructBlock` 在构造时需要传入当前构造的 `Block` 批次数 `n`，这里将预先构造出
                        // `n` 的序列并打乱顺序，以期在生成 `Block` 时能更具有迷惑性和随机性，但又不在数量和稳
                        // 定性上影响整体构造逻辑。
                        let mut seq = (0..Self::QUEUE_SIZE).collect::<Vec<usize>>();
                        seq.shuffle(&mut rand::thread_rng());

                        // 通过 `ConstructBlock` trait 构建新的 `Block`，并全部推送至 `queue` 队列中，新
                        // 生成的 next `Cursor` 将被用于创建 `Block` 中的元素 T。
                        for n in seq {
                            let block = T::construct_block(n, next);

                            // `Err` 表示队列已满，剩余内容不再推送（实际场景中应为所有 `Block` 均应被推送至
                            // 队列中，不会存在队列已满的情况）
                            if queue.push(block).is_err() {
                                break;
                            }

                            // 在成功推送至少一条 `Block` 后，立刻唤醒等待的 `Future` 以实现快速响应，通过
                            // `Option::take` 实现，在完成 take 后，`Option` 中便无 `Waker` 可唤醒。
                            if let Some(waker) = waker.take() {
                                waker.wake_by_ref();
                            }

                            debug_assert!(waker.is_none());
                        }

                        state.store(false);
                    }

                    if let Some(waker) = waker {
                        waker.wake_by_ref();
                    }
                }
            }),
        })
    }
}

/// `Block` 表示预先分配的 size=Block::SIZE 的数组，提供 Block::SIZE 个目标元素，通常而言 `Block` 应在
/// `thread_local` 中依赖线程的创建进行获取。
#[derive(Debug, Copy, Clone)]
pub struct Block<T> {
    /// `index` 表示当前 `Block` 的生成进度，当 `index` 超过 size 时则不再生成 T。
    index: usize,

    /// 使用 `array` 而非 `Vec` 来存储 T，因为在大多数场景下，T 满足 T: Copy，
    /// 在栈上分配空间以提高效率。
    array: [T; Block::<()>::SIZE],
}

impl<T> Block<T> {
    pub(crate) const SIZE: usize = 8;

    pub(crate) fn new(array: [T; 8]) -> Self {
        Block { index: 0, array }
    }
}

impl<T> From<[T; Block::<()>::SIZE]> for Block<T> {
    fn from(array: [T; Block::<()>::SIZE]) -> Self {
        Self::new(array)
    }
}

impl<T: Clone> Iterator for Block<T> {
    type Item = T;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= Self::SIZE {
            return None;
        }

        // 使用 T: Clone 而非 T: Copy 来实现 `Iterator`，以满足所有场景下的 T 类型。
        // （当 T 是 Copy 类型时，T::clone 与 Copy 等价）
        let current = self.array[self.index].clone();

        self.index += 1;
        Some(current)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remain = Self::SIZE - self.index;
        (remain, Some(remain))
    }
}

/// `BlockFuture` 代表发放 `Block` 的异步任务，当队列内的 `Block` 不足时，会通过额外的线程补充
/// 队列内容，并返回 `Pending`，其余情况则返回 `Ready`。
struct BlockFuture<T> {
    /// 继承自 `BlockFrame` 的 `queue` 队列。
    queue: Arc<ArrayQueue<Block<T>>>,

    /// `supply` 表示当 Future 返回 `Pending` 时，应该执行的补充队列的操作, `supply` 获取一个 `Waker`
    /// 引用，应确保调用完毕时，执行 `Waker::wake_by_ref` 操作。
    /// （使用 `Waker` 引用的目的是为之后可能产生的其他有关 waker 的操作预留扩展空间，如果接受的是带有所有权
    /// 的 `Waker`，有可能出现所有权纠纷）
    /// （使用 `Option<&Waker>` 的原因是为了实现只唤醒一次的特性，详见 `BlockFrame::next_block` 中的注释）
    supply: Arc<dyn Fn(Option<&Waker>) + Send + Sync + 'static>,
}

impl<T> Future for BlockFuture<T> {
    type Output = Block<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // 从队列获取 `Block`
        if let Some(block) = self.queue.pop() {
            return Poll::Ready(block);
        }

        // 当 `queue` 队列中无 `Block` 时，代表当前时间段内所有 `Block` 都已经发放， 并且尚未回收，
        // 等待该段时间间隔后重新尝试获取队列内容。
        {
            let waker = cx.waker().clone();
            let supply = Arc::clone(&self.supply);
            thread::spawn(move || supply(Some(&waker)));
        }

        Poll::Pending
    }
}

/// `Cursor` 用于表示一个时间锚点
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Cursor(u32);

impl Cursor {
    pub(crate) fn into_inner(self) -> u32 {
        self.0
    }
}

impl Cursor {
    // 用于计算时间戳的基准起始时间 '2021-12-10 12:27:33'
    const TIMEBASE: u64 = 1639110453;

    pub fn new() -> Self {
        lazy_static! {
            // `START` 用于表示当前进程的起始时间，用于计算时间间隔
            static ref START: Instant = Instant::now();

            // `TIMESTAMP` 代表秒级别的 UNIX 时间戳，通过 `SystemTime` 计算获得，如果在计算时间戳时发生了时间
            // 倒转，则会在初始化时 panic，所获得的时间戳会由 u64 类型转化为 u32 类型，并做对应的溢出检测，溢出
            // 的时间将会引发 panic。
            //
            // （该时间戳的基准时间点并非 '1970-01-01 00:00:00'，而是 `Cursor::TIMEBASE`）
            static ref TIMESTAMP: u32 = {
                let mut timestamp = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
                    Ok(duration) => duration.as_secs(),
                    Err(error) => {
                        panic!("error occur when initializing `Cursor::TIMESTAMP`: {}", error)
                    }
                };

                // 为了能支撑更长久的程序运行周期，因此以 `TIMEBASE` 为截断点，仅计算此时间之后的时间戳，`u32`
                // 类型的秒级时间戳理论上能支撑程序运行 100+ 年。
                timestamp -= Cursor::TIMEBASE;

                if timestamp > u32::MAX as u64 {
                    panic!("`Cursor::TIMESTAMP` overflows over u32")
                }

                timestamp as u32
            };
        }

        // `elapsed` 代表从进程开始到当前的时间间隔，该时间间隔会由 u64 类型转化为 u32 类型，并做溢出检测。
        let elapsed = START.elapsed().as_secs();
        if elapsed > u32::MAX as u64 {
            panic!("`elapsed` overflows over u32 on Cursor::new()");
        }

        // 提供秒级别的游标控制，每个游标之间的间隔为 1 秒
        Cursor(
            TIMESTAMP
                .checked_add(elapsed as u32)
                .expect("`TIMESTAMP + elapsed` overflows over u32 on Cursor::new()"),
        )
    }

    /// `next` 方法将在新的时间线（秒）创建 `Cursor`，其内部实现为通过 loop 自旋不断地尝试获取 `Cursor`，当
    /// 新生成的 `Cursor` 大于当前 `Cursor` 时结束自旋，并返回新的 `Cursor`。
    pub fn next(self) -> Self {
        let backoff = Backoff::new();
        loop {
            let next = Self::new();
            if next > self {
                return next;
            }

            // 使用 `snooze` 而非 `spin`，在一秒的间隔内挂起当前线程也许已经足够让 CPU 处理更多其他内容，可能是
            // 比 `spin` 自旋更好的选择。
            backoff.snooze();
        }
    }
}

impl Default for Cursor {
    fn default() -> Self {
        Self::new()
    }
}

/// `ConstructBlock` 用于从 T 构建一个 Block，使用此方法可以快速构建一个包含 Block::SIZE 个元素的 `Block<T>`。
/// 通常而言，在实现此方法时，需要先构建一个 `[T; 8]`，再使用 `new` 或者 `Into` trait 完成对 `Block<T>`
/// 的构建。
pub trait ConstructBlock: Sized {
    /// `n` 代表是对 `Block` 的第 N 次创建, 0 <= n < BlockFrame::QUEUE_SIZE。`cursor` 代表当前的时间锚点。
    fn construct_block(n: usize, cursor: Cursor) -> Block<Self>;
}
