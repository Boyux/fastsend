use crate::Serialer;
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use thiserror::Error;

/// `InspectFnMut` 是 Inspect 方法的快捷方式（alias），由于需要兼顾异步任务及多线程场景，因此 Inspect
/// 方法的签名会非常长，使用 `InspectFnMut` 来减少代码长度。
pub type InspectFnMut<E> = Box<
    dyn FnMut(&str) -> Pin<Box<dyn Future<Output = Result<bool, E>> + Send + 'static>>
        + Send
        + 'static,
>;

/// `TicketSerialer` 是一个可配置的、生成字母+数字组合的序列号生成器，可用于生成各类编码，如设备、资产、事件等。
/// 其借助外部系统来确保序列号的唯一性，当通过 `inspect` 方法校验序列号为重复时，`TicketSerialer` 会向后借用
/// 一秒来重新构建序列号，以期找到唯一序列号值，这个过程会重复 `retry_times` 次，若仍未找到唯一序列号，则会返回
/// `MaxRetry` 错误。
pub struct TicketSerialer<E> {
    /// ============ 配置项 ===============

    /// 短号模式，缺省模式生成 'XXXX-XXXXX-XXXXX-XXXXX-XX' 格式的序列号（与 UUID 类似但和 UUID 不一样），短号
    /// 模式则生成 'XXXX-XXXXX-XXXXX' 格式的序列号（去除了设备 ID 和校验码等信息）。
    short_repr: bool,

    /// 连接符：是否使用连接符连接序列号的各部分，缺省配置是 true。
    minus_sep: bool,

    /// 小写字符：是否使用小写字符构建序列号，缺省配置是 false。
    lowercase: bool,

    /// 十进制：是否使用十进制构建 `decimal_digit_part`，缺省配置是 true。
    decimal_only: bool,

    /// 重试次数：调用 `inspect` 方法校验序列号唯一性的重试次数，缺省配置是 10。
    retry_times: usize,

    /// ============ 临时存储 ===============
    /// 调用 `feed` 方法时，数据临时存储于 `data` 字段，在调用 `init` 方法后将生成下方的序列号构建参数。
    data: Vec<u8>,

    /// ============ InspectFnMut ===============
    /// `inspect` 用于校验生成的序列号是否是合法的序列号（即是否是唯一的，是否与已经生成的序列号重复），借助外部
    /// 系统实现。`TicketSerialer` 将序列号唯一性保证委托外部系统完成，其实现方式就是通过 `inspect` 方法。
    ///
    /// （使用 `FnMut` 的原因在于外部实现有可能需要引用环境中的可变变量，如 sqlx 中的 `Executor`）
    inspect: InspectFnMut<E>,

    /// ============ 生成参数 ===============

    /// 日期：用于生成序列好的头两个部分，分别为代表年月日的四字符部分（'XXXX'）和代表时分秒的五字符部分（'XXXXX'）
    datetime: Option<DateTime<Local>>,

    /// 中间数字序列：通常代表环境或硬件信息，如线程、进程号，设备号等，缺省生成纯数字十进制字符
    decimal_digit_part1: Option<u16>,

    /// 尾部数字序列：处于尾部的字节将被收集到尾部数字序列中，将会以两个字节一组合成 u16 形式的数字，并默认转化为纯
    /// 数字十进制字符，没有数量限制。
    decimal_digit_part2: Vec<u8>,

    /// 校验码
    auth: u8,
}

impl<E> Debug for TicketSerialer<E> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketSerialer")
            .field("short_repr", &self.short_repr)
            .field("minus_sep", &self.minus_sep)
            .field("lowercase", &self.lowercase)
            .field("decimal_only", &self.decimal_only)
            .field("retry_times", &self.retry_times)
            .field("data", &self.data)
            .field(
                "inspect",
                &"FnMut(&str) -> impl Future<Output = Result<(), E>>",
            )
            .field("datetime", &self.datetime)
            .field("decimal_digit_part1", &self.decimal_digit_part1)
            .field("decimal_digit_part2", &self.decimal_digit_part2)
            .field("auth", &self.auth)
            .finish()
    }
}

impl<E> TicketSerialer<E> {
    pub fn new<F>(f: F) -> Self
    where
        F: FnMut(&str) -> Pin<Box<dyn Future<Output = Result<bool, E>> + Send + 'static>>
            + Send
            + 'static,
    {
        TicketSerialer {
            short_repr: false,
            minus_sep: true,
            lowercase: false,
            decimal_only: true,
            retry_times: 10,
            data: Vec::with_capacity(8),
            inspect: Box::new(f),
            datetime: None,
            decimal_digit_part1: None,
            decimal_digit_part2: Vec::with_capacity(5),
            auth: u8::MAX,
        }
    }

    pub fn short_repr(mut self) -> Self {
        self.short_repr = true;
        self
    }

    pub fn no_sep(mut self) -> Self {
        self.minus_sep = false;
        self
    }

    pub fn lowercase(mut self) -> Self {
        self.lowercase = true;
        self
    }

    pub fn alphabet(mut self) -> Self {
        self.decimal_only = false;
        self
    }

    pub fn retry_times(mut self, n: usize) -> Self {
        self.retry_times = n;
        self
    }

    /// `init` 方法将保存在 `data` 中的数据转换为对应的构建参数，需要注意的是，如果 `data` 中的字节数不足 8 个
    /// 字节，那么 `init` 方法会强行按八个字节进行构建，缺少的部分将被缺省地补充为 0，因此请务必保证 feed 超过 8
    /// 个字节的数据，不然生成的序列号有可能重复（极大概率）。
    fn init(&mut self) {
        /// 对 auth 进行混淆计算
        #[inline]
        fn mix(auth: &mut u8, b: u8) {
            *auth = auth.rotate_left(5) ^ b;
        }

        /// 用于从 data 中逐个获取字节，如果 data 中无数据，则返回 0
        #[inline]
        fn next<'a>(mut iter: impl Iterator<Item = &'a u8>, auth: &mut u8) -> u8 {
            let item = iter.next().copied().unwrap_or_default();
            mix(auth, item);
            item
        }

        let mut iter = self.data.iter();

        self.datetime = {
            let ts = u32::from_be_bytes([
                next(&mut iter, &mut self.auth),
                next(&mut iter, &mut self.auth),
                next(&mut iter, &mut self.auth),
                next(&mut iter, &mut self.auth),
            ]);

            Some(Local.timestamp(ts as i64, 0))
        };

        self.decimal_digit_part1 = {
            let ddp1 = u16::from_be_bytes([
                next(&mut iter, &mut self.auth),
                next(&mut iter, &mut self.auth),
            ]);

            Some(ddp1)
        };

        // 保证 `decimal_digit_part2` 有至少两个字节
        self.decimal_digit_part2
            .push(next(&mut iter, &mut self.auth));
        self.decimal_digit_part2
            .push(next(&mut iter, &mut self.auth));

        // 多余部分直接 append 到 `decimal_digit_part2` 末尾
        while let Some(&item) = iter.next() {
            mix(&mut self.auth, item);
            self.decimal_digit_part2.push(item);
        }
    }
}

#[derive(Debug, Error)]
pub enum TicketSerialError<E> {
    #[error("an error occurs when inspecting new-generated ticket: {0}")]
    InspectFailed(
        #[from]
        #[source]
        E,
    ),

    #[error("reach max retry times while generating ticket")]
    MaxRetry,

    #[error("given data is not enough to build a ticket")]
    DataNotEnough,
}

impl<E> Serialer for TicketSerialer<E>
where
    E: 'static,
{
    type Output = String;

    type Error = TicketSerialError<E>;

    fn build(
        mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Self::Output, Self::Error>> + Send + 'static>> {
        self.init();
        Box::pin(async move {
            let mut cnt = 0;
            let mut secs = 0;
            loop {
                cnt += 1;
                if cnt >= self.retry_times {
                    return Err(TicketSerialError::MaxRetry);
                }

                (!self.decimal_digit_part2.is_empty())
                    .then(|| ())
                    .ok_or_else(|| TicketSerialError::DataNotEnough)?;

                let dt = self
                    .datetime
                    .as_ref()
                    .cloned()
                    .map(|old| old + Duration::seconds(secs))
                    .ok_or_else(|| TicketSerialError::DataNotEnough)?;

                let (head, left, right, tail, auth) = (
                    build_head(&dt),
                    build_left(&dt),
                    self.decimal_digit_part1
                        .map(|n| format_u16(n, self.decimal_only))
                        .ok_or_else(|| TicketSerialError::DataNotEnough)?,
                    self.decimal_digit_part2
                        .chunks(2)
                        .map(to_u16)
                        .map(|n| format_u16(n, self.decimal_only))
                        .fold(String::with_capacity(5), |prev, next| prev + &next),
                    to_string_radix((self.auth % u8::MAX) as usize, 26, 2, false),
                );

                let sep = if self.minus_sep { "-" } else { "" };

                let mut output: String = if !self.short_repr {
                    [&*head, &*left, &*right, &*tail, &*auth].join(sep)
                } else {
                    [&*head, &*left, &*tail].join(sep)
                };

                if self.lowercase {
                    output.make_ascii_lowercase();
                }

                match (self.inspect)(&*output).await {
                    Ok(duplicated) if duplicated => {
                        secs += 1;
                        continue;
                    }
                    Ok(_) => (),
                    Err(e) => return Err(e.into()),
                }

                return Ok(output);
            }
        })
    }

    fn feed(&mut self, data: &[u8]) {
        self.data.extend_from_slice(data);
    }
}

#[inline]
fn to_u16(s: &[u8]) -> u16 {
    assert!(s.len() > 0 && s.len() <= 2);
    if s.len() == 2 {
        u16::from_be_bytes([s[0], s[1]])
    } else {
        s[0] as u16
    }
}

#[inline]
fn format_u16(n: u16, decimal_only: bool) -> String {
    if decimal_only {
        format!("{:05}", n)
    } else {
        to_string_radix(n as usize, 36, 4, true)
    }
}

#[inline]
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

fn build_head(dt: &DateTime<Local>) -> String {
    const OFFSET: i32 = 1918;
    format!(
        "{}{}{}",
        to_string_radix((dt.year() - OFFSET) as usize, 26, 2, false),
        to_string_radix((dt.month() - 1) as usize, 12, 1, true),
        to_string_radix((dt.day() - 1) as usize, 31, 1, true)
    )
}

fn build_left(dt: &DateTime<Local>) -> String {
    format!(
        "{}{}{}",
        to_string_radix(dt.hour() as usize, 24, 1, true),
        to_string_radix(dt.minute() as usize, 36, 2, true),
        to_string_radix(dt.second() as usize, 36, 2, true)
    )
}
