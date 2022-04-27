fastsend.rs
===========

# 分布式 ID 和序列号生成方案

由于一些众所周知的原因，越来越复杂的环境要求我们需要一种不依赖或尽可能少依赖外部环境的 ID 和序列号生成方案，
最早且最知名的解决方案是 Twitter 提出的雪花 ID 生成方案（Snowflake ID 生成算法）。但对于 Snowflake，
我个人对其中的一些实现方式并不是很满意，包括但不限于：

    1. 使用毫秒级时间戳，预计可用年限为 40 年，而我对程序可用性的要求是至少 100 年（虽然我可能一定活不到 100 岁）；
    2. Snowflake 的设计是为特别大量的并发考虑的，每台机器每毫秒可生成 1000 个 ID，而这也是我所不需要的（也因此
       我在设计 fastsend 时，仅要求每秒生成 65536 个 ID，相差了 100 个数量级）；
    3. 实时生成并无必要，完全可以预先生成，按需分配（这种实现的一个必要条件在于，我们并不需要从 ID 中获取任何业务
       相关的信息，仅只作为一个唯一标识符，如果需要包含业务信息，应使用序列号）；

基于以上几点，我设计了一个简单但又非常实用（对我而言）的 ID 生成方案，不需要借助外部系统（如数据库、缓存等），依靠
时间和自增序列，以及一些辅助信息（如线程 ID、进程 ID 和设备号等）来生成一个全局唯一的 ID。同时也提供了序列号生成
的实现方式，但相较而言，ID 更快更高效，更具有通用性（序列号的生成从实现角度上说也依赖于 ID 生成）。

## ID

ID 被定义为一个 64 位无符号整数，没有业务含义，常用作数据库主键，整数作为 ID 可进行高效率的比较操作，这通常用于在
数据量大的查询场景进行查询速度的优化（虽然字符串也能进行比较，但就效率而言，整数比较要比字符串比较效率更高、速度更快）。
分布式 ID 生成并不需要依赖外部系统，仅有算法本身加上时间戳即可生成一个全局唯一的 ID，这也从另一方面强调了 ID 生成
的特性：速度快！持续性地生成可靠的 ID 是 fastsend 的核心功能，fastsend 把 ID 抽象为一个独立的 trait（并为 ID
提供了默认实现 Token）其表现形式为：

```rust
pub trait ID {
    fn id(self) -> u64;
}
```

这么做的目的在于：也许在不远的将来，会有其他 ID 生成方式，届时 fastsend 将提供更多样的 ID 生成方式。

## Serial

Serial 即序列号，其定义为：一串包含特定业务含义的字符串。序列号和 ID 一样，都具有唯一标识性，但序列号包含业务信息，
我们可以从序列号中解析出特定的业务信息（而这恰是 ID 所不具备的），正是由于序列号的业务性，因此生成序列号也比生成 ID
要更加复杂，甚至需要借助外部系统来完成。考虑到通常情况下生成序列号可能包括必要的 IO 操作，Serialer trait 被设计成
返回一个 Future 以便拟合 Rust 的异步任务系统。fastsend 提供了 Serialer 的默认实现 TimeSerialer，并且为 Token
实现了 Serial 以供组合使用。

```rust
use std::pin::Pin;
pub trait Serialer {
    type Output: Display;
    
    fn build(self) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'static>>;
    
    fn feed(&mut self, data: &[u8]);
}

pub trait Serial {
    fn serial<S: Serialer>(self, serialer: &mut S);
}
```

关于 Serialer 和 Serial 详细的说明请参照这两个 trait 的注释。

# fastsend 注意事项

## feature

fastsend 提供了 'pause_on_start' feature，用于在程序启动时暂停至一个新的时间节点，以避免造成 ID 和序列号生成
冲突，这在代码注释中有详细说明。pause_on_start 是默认 feature，在分布式场景下，pause_on_start 是保证 fastsend
稳定可靠的重要保证之一。但在命令行程序中，pause_on_start 可能会不可避免地造成程序响应时间过长的问题，因此在命令行
应用中，禁用掉默认 feature 是一个正确的选择，但此时必须由调用方来额外确认生成的 ID 或序列号是否是全局唯一的。

## 环境变量

fastsend 需要配置两个环境变量，分别是 `FASTSEND_RANDOM_VALUE` 和 `FASTSEND_DEVICE_ID`，分别在编译时和运行时
使用。

其中，'FASTSEND_RANDOM_VALUE' 用于编译时确定程序中的随机数基础，这个随机数基础指的是：该随机数会用于在程序中
对各项数值进行混淆（包括环境变量 'FASTSEND_DEVICE_ID'），避免被攻击者嗅探到程序中的某些特定值。'FASTSEND_RANDOM_VALUE'
是程序必须提供的值，如果未在环境变量中提供该值，则程序被视为单应用程序，将通过 'rand' 生成一个随机值。注意，如果任由
程序生成随机值，并部署在多设备环境，可能导致最终产生的设备号 'FASTSEND_DEVICE_ID' 重复，造成 ID 生成重复。

'FASTSEND_DEVICE_ID' 则是用于在运行时确定设备唯一 ID 的环境变量，用于在分布式环境中确定设备唯一性。如果 fastsend 程序应用于
多多设备（分布式）环境，那么你应该保证每台设备上的 'FASTSEND_DEVICE_ID' 都互相独立，各不相同，对于 'FASTSEND_DEVICE_ID'
的读取是在程序运行时，但仅会在程序初始化时读取一次。设备 ID 在程序运行过程中不是必须的，因此它以 `Option` 的形式存在于代码中，
在单应用程序中，如果不想提供设备 ID，那么当程序监测到无 'FASTSEND_DEVICE_ID' 环境变量时，会采用其他方式构建 ID（例如进程号），
因此在多设备场景如果未提供设备 ID，那么很可能造成 ID 生成重复。设备 ID 会使用上述提到的 'FASTSEND_RANDOM_VALUE' 值进行混淆
以避免设备号被恶意嗅探。

# TODO

- [ ] 实现更多 `Serialer`
  - [X] `TicketSerialer`
  - [X] `UUIDSerialer`
  - [X] `Random62Serialer`