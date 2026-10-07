//! Millisecond timer that works both in the browser and in native tests.
//!
//! `std::time::Instant` panics on `wasm32-unknown-unknown`, so there the clock
//! is `performance.now()`.

#[cfg(target_arch = "wasm32")]
mod imp {
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = performance)]
        fn now() -> f64;
    }

    pub struct Clock(f64);

    impl Clock {
        pub fn start() -> Self {
            Self(now())
        }
        pub fn elapsed_ms(&self) -> f64 {
            now() - self.0
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod imp {
    use std::time::Instant;

    pub struct Clock(Instant);

    impl Clock {
        pub fn start() -> Self {
            Self(Instant::now())
        }
        pub fn elapsed_ms(&self) -> f64 {
            self.0.elapsed().as_secs_f64() * 1000.0
        }
    }
}

pub use imp::Clock;
