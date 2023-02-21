macro_rules! cfg_tracing {
    ($($item:item)*) => {
        $(
            #[cfg(feature = "tracing")]
            $item
        )*
    };

    ($($item:tt)*) => {
        $(
            #[cfg(feature = "log")]
            $item
        )*
    }
}

macro_rules! cfg_log {
    ($($item:item)*) => {
        $(
            #[cfg(feature = "log")]
            $item
        )*
    };

    ($($item:tt)*) => {
        $(
            #[cfg(feature = "log")]
            $item
        )*
    }
}
