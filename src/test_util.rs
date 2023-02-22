macro_rules! assert_ready {
    ($fut:expr) => {{
        let mut fut = $fut;
        let pinned = std::pin::Pin::new(&mut fut);
        match futures_util::poll!(pinned) {
            std::task::Poll::Ready(val) => val,
            std::task::Poll::Pending => panic!("expected Ready, got Pending"),
        }
    }};
}

macro_rules! assert_pending {
    ($fut:expr) => {{
        let mut fut = $fut;
        let pinned = std::pin::Pin::new(&mut fut);
        match futures_util::poll!(pinned) {
            std::task::Poll::Ready(_) => panic!("expected Pending, got Ready"),
            std::task::Poll::Pending => {}
        }
    }};
}

macro_rules! assert_some {
    ($opt:expr) => {{
        match $opt {
            Some(val) => val,
            None => panic!("expected Some, got None"),
        }
    }};
}

macro_rules! assert_none {
    ($opt:expr) => {{
        match $opt {
            Some(_) => panic!("expected None, got Some"),
            None => {}
        }
    }};
}

pub(crate) use {assert_ready, assert_pending, assert_some, assert_none};
