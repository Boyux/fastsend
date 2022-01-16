//! 关于 fastsend 的详细说明，请查看 README.md
#![doc = include_str!("../README.md")]

#[doc(hidden)]
pub mod block;
pub use block::{Block, BlockFrame, ConstructBlock, Cursor};

#[doc(hidden)]
pub mod token;
pub use token::Token;

#[doc(hidden)]
pub mod serial;
pub use serial::{Serial, Serialer, TimeSerialer};

#[cfg(feature = "ticket")]
pub use serial::{ticket::TicketSerialError, ticket::TicketSerialer};

/// `ID` 是 fastsend 的核心 trait，用于生成不重复的 id，其表示形式为 64 位无符号整数，可用作数据库的主键。
/// 其生成方法会消耗自身所有权，目的是确保一个 `ID` 实例只生成一个 id，多次生成在某些特定场景下会造成 id 冲突
/// 的情况，例如因为代码逻辑错误导致多次调用 `id` 方法（但实际上如果 `ID` 是 Copy 的，这种情况也很难避免）。
///
/// 需要注意的是 id 生成是被定义为一定成功的，并且是一个普通方法而非异步方法，意味着：
///     1. ID 必须一定能转化为 u64，不允许出现失败，但允许在生成的过程中 panic（这是无法阻止的）；
///     2. ID 生成不允许是异步任务，即不允许有外部调用依赖，例如数据库、缓存、磁盘 IO 等操作，必须连贯完成，这也从
///        另一个角度表明 ID 生成应该尽可能的轻量化（这是与 `Serialer` trait 不同的地方，`Serialer` 允许使用
///        异步任务，也允许有外部依赖）。
pub trait ID {
    /// # 一个不成熟的想法
    ///
    /// 考虑把 `self` 替换成 `Box<Self>` 来显式地表明 `id` 方法会消耗掉自身，并且这在 `Self` 是 Copy 的情况
    /// 下也同样奏效，但这里就多了一次堆分配，在特定场景可能会形成性能瓶颈。签名形如：
    ///
    /// ```no_run
    /// pub trait ID {
    ///     fn id(self: Box<Self>) -> u64;
    /// }
    /// ```
    fn id(self) -> u64;
}

use lazy_static::lazy_static;
use std::cell::RefCell;

lazy_static! {
    /// 用作全局变量的 `BlockFrame` 支持在多线程环境下持续生成 `Block`，并会提前缓存一部分预生成的 `Block`，
    /// 通常而言一个程序仅需要一个全局 `BlockFrame`，`fastsend::next_token` 的实现中就依赖于这个包裹在
    /// `lazy_static` 里的全局生成器。全局生成器不需要（也不应该）直接被调用方使用，其会通过一个 `thread_local`
    /// 暴露给使用者（fastsend 的 `thread_local` 就被包裹在 `with_block` 函数内），关于 `thread_local`
    /// 的相关信息，详见 `with_block` 内的说明。
    static ref FRAME: BlockFrame<Token> = BlockFrame::new();
}

/// `next_token` 是 fastsend 中获取 `Token` 的主要方式，其会从当前线程持有的 `Block` 中获取一个 `Token` 并
/// 返回给调用方，由于 `with_block` 使用了 `thread_local`，因此 `next_block` 方法是线程安全且无锁竞争的（这里
/// 对一个函数强调了线程安全，是因为在函数实现的内部使用了全局变量，即 `BlockFrame`）。
pub async fn next_token() -> Token {
    with_block(|block| {
        // 对 `Block` 可用性的额外保障，确保 `Block` 仍然可以生成 `Token`。
        // （`<Block as Iterator>::size_hint` 用于表明 `Block` 剩余可生成的元素数量）
        debug_assert!(block.size_hint().0 > 0);

        block.next().expect("unexpected drained `Block` iterator")
    })
    .await
}

/// `with_block` 是一个辅助方法，用于从 thread_local 中获取本线程拥有的 `Block`，由于是使用了 `RefCell` 来
/// 获取可变引用，因此这里是传入一个 `FnOnce` 来完成对 `Block` 的操作（主要原因也在于 `RefMut<T>` 产生的可变
/// 引用 `&mut T` 由于生命周期约束的原因，无法移动到函数外部），因此这是一种对 `&mut Block` 的折中的使用方式。
///
/// `with_block` 可以保证传递给 `f` 的 `Block` 一定是包含可用元素的，即调用 `<Block as Iterator>::next` 方
/// 法时，返回的一定是 `Some`。
async fn with_block<T, F>(f: F) -> T
where
    F: FnOnce(&mut Block<Token>) -> T,
{
    thread_local! {
        /// `BLOCK` 是对 `Token` 的第二次预分配行为，此次预分配是各线程各自的预分配，即在 `thread_local`
        /// 里获取 `BLOCK`，此时再次从 `Block` 里获取元素便不需要加锁，避免了锁竞争。
        ///
        /// 这里使用了 `RefCell` 来实现内部可变性，由于从 `Block` 中获取元素以及更新 `Block` 都需要可变引用，
        /// 因此不得不套一层 `RefCell`，虽然有性能损耗，但从宏观上说这也是必须要有的消耗，也避免了使用 unsafe。
        static BLOCK: RefCell<Option<Block<Token>>> = RefCell::new(None);
    }

    // 在两种情况下需要重新从 `BlockFrame` 获取新生成的 `Block`：
    //     - 当前线程的 `BLOCK` 尚未初始化时，即 `BLOCK` 内部为 `None` 时
    //     - 当前线程的 `BLOCK` 内部的 `Token` 已全部消耗完毕时，需要重新获取
    let block_should_assign = BLOCK.with(|block| {
        0 == block
            .borrow()
            .as_ref()
            // 通过 `size_hint` 来判断剩余可生成的 `Token` 数量
            .map(|block| block.size_hint().0)
            // 当 `BLOCK` 为 `None` 时，`unwrap_or_default` 同样会返回 0
            .unwrap_or_default()
    });

    if block_should_assign {
        // `next_block` 作为异步函数的分隔点，函数过程将在此处被阻断并保留，因此如果在此之前已经出现了对
        // `block` 的可变借用（即调用了 `borrow_mut` 方法），则在出让当前 `Future` 后，该可变借用仍然
        // 存在，Runtime 在调度其他 `Future` 时再次调用了 `borrow_mut` 方法，会造成多次借用错误，产生
        // 程序 `panic`。
        // 为避免这个问题，将 `borrow_mut` 的调用延后至 `next_block` 之后，在异步任务断点之前不会有任何
        // `Future` 抢占可变借用，确保该异步函数过程顺利完成。
        // （由于异步任务的可调度性，以上问题在同一个线程中也同样会出现。）
        let next_block = FRAME.next_block().await;

        // ===============================================================

        BLOCK.with(|block| *block.borrow_mut() = Some(next_block));
    }

    BLOCK.with(|block| f(block.borrow_mut().as_mut().unwrap()))
}

use std::env;

lazy_static! {
    /// `RV` 是用于对设备号进行混淆的参数，通常而言由编译时的环境变量 `FASTSEND_RANDOM_VALUE` 控制，如果未提供
    /// 该环境变量，则视为单点设备，此时使用随机数生成一个 `RV`。需要注意的是，如果是多设备场景使用，请一定记得在编译
    /// 时提供该环境变量，不然在运行时生成随机数可能导致设备号冲突。
    static ref RV: u8 = option_env!("FASTSEND_RANDOM_VALUE")
        .map(|var| var.parse::<u8>().ok())
        .flatten()
        .unwrap_or_else(rand::random);

    /// 用于定位设备的设备号（添加了随机要素 `RV`），从环境变量中获取，在 id 和 serial 生成的场景用来避免多设备
    /// 冲突，使用 `lazy_static` 来确保环境变量在整个程序周期只会被获取一次。
    pub static ref DEVICE_ID: Option<u8> = env::var("FASTSEND_DEVICE_ID")
        .map(|var| var.parse::<u8>().ok())
        .ok()
        .flatten()
        .map(|mut id| {
            id = id.rotate_left(3) ^ (*RV);
            id = id.rotate_left(3) ^ (*RV);
            id = id.rotate_left(3) ^ (*RV);
            id
        });
}
