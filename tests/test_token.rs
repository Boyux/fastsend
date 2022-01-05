use fastsend::ID;
use fastsend::{Serial, Serialer, TimeSerialer};
use futures::future;
use std::collections::HashSet;
use std::error::Error;
use std::result::Result as StdResult;

type Result<T> = StdResult<T, Box<dyn Error>>;

const TOP: usize = 10999;

macro_rules! into_hashset {
    (@it $iter:expr) => {{
        IntoIterator::into_iter($iter).collect::<HashSet<_>>()
    }};

    (@ok $iter:expr) => {{
        IntoIterator::into_iter($iter)
            .collect::<StdResult<Vec<_>, _>>()?
            .into_iter()
            .collect::<HashSet<_>>()
    }};
}

#[tokio::test]
async fn test_unique_id() {
    let set = into_hashset!(@it
        future::join_all((0..TOP).map(|_| async { fastsend::next_token().await.id() })).await
    );

    assert_eq!(set.len(), TOP);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 10)]
async fn test_unique_id_concurrently() -> Result<()> {
    let set = into_hashset!(@ok
        future::join_all(
            (0..TOP).map(|_| {
                tokio::spawn(async move { fastsend::next_token().await.id() })
            })
        )
        .await
    );

    assert_eq!(set.len(), TOP);

    Ok(())
}

macro_rules! serial {
    ($expr:expr => $serialer:ty) => {{
        let mut serialer = <$serialer>::default();
        $expr.serial(&mut serialer);
        serialer.build().await
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 10)]
async fn test_unique_serial_concurrently() -> Result<()> {
    let set = into_hashset!(@ok
        future::join_all(
            (0..TOP).map(|_| {
                tokio::spawn(async move { serial!(fastsend::next_token().await => TimeSerialer) })
            })
        )
        .await
    );

    assert_eq!(set.len(), TOP);

    Ok(())
}
