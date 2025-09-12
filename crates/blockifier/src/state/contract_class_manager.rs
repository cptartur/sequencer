pub const DEFAULT_COMPILATION_REQUEST_CHANNEL_SIZE: usize = 2000;

#[cfg(all(feature = "cairo_native", feature = "cairo_native_execution"))]
pub use crate::state::native_class_manager::NativeClassManager as ContractClassManager;

#[cfg(all(not(feature = "cairo_native"), not(feature = "cairo_native_execution")))]
pub mod trivial_class_manager {
    #[cfg(any(feature = "testing", test))]
    use cached::Cached;
    use starknet_api::core::ClassHash;

    use crate::blockifier::config::ContractClassManagerConfig;
    use crate::execution::contract_class::RunnableCompiledClass;
    use crate::state::global_cache::{CompiledClasses, RawClassCache};

    #[derive(Clone)]
    pub struct TrivialClassManager {
        cache: RawClassCache,
    }

    // Trivial implementation of the class manager for Native-less projects.
    impl TrivialClassManager {
        pub fn start(config: ContractClassManagerConfig) -> Self {
            assert!(
                !config.cairo_native_run_config.run_cairo_native,
                "Cairo Native feature is off."
            );

            Self { cache: RawClassCache::new(config.contract_cache_size) }
        }

        pub fn get_runnable(&self, class_hash: &ClassHash) -> Option<RunnableCompiledClass> {
            Some(self.cache.get(class_hash)?.to_runnable())
        }

        pub fn set_and_compile(&self, class_hash: ClassHash, compiled_class: CompiledClasses) {
            self.cache.set(class_hash, compiled_class);
        }

        pub fn clear(&mut self) {
            self.cache.clear();
        }

        #[cfg(any(feature = "testing", test))]
        pub fn get_cache_size(&self) -> usize {
            self.cache.lock().cache_size()
        }
    }
}

#[cfg(all(not(feature = "cairo_native"), not(feature = "cairo_native_execution")))]
pub use trivial_class_manager::TrivialClassManager as ContractClassManager;

#[cfg(all(not(feature = "cairo_native"), feature = "cairo_native_execution"))]
pub mod mock_class_manager {
    use starknet_api::core::ClassHash;

    use crate::blockifier::config::ContractClassManagerConfig;
    use crate::execution::contract_class::RunnableCompiledClass;
    use crate::state::global_cache::{CompiledClasses, RawClassCache};

    pub struct MockClassManager;

    impl MockClassManager {
        pub fn start(config: ContractClassManagerConfig) -> Self {
            Self
        }

        pub fn get_runnable(&self, class_hash: &ClassHash) -> Option<RunnableCompiledClass> {
            unimplemented!("Native compilation is not supported")
        }

        pub fn set_and_compile(&self, class_hash: ClassHash, compiled_class: CompiledClasses) {
            unimplemented!("Native compilation is not supported")
        }

        pub fn clear(&mut self) {
            unimplemented!("Native compilation is not supported")
        }

        #[cfg(any(feature = "testing", test))]
        pub fn get_cache_size(&self) -> usize {
            unimplemented!("Native compilation is not supported")
        }
    }
}

#[cfg(all(not(feature = "cairo_native"), feature = "cairo_native_execution"))]
pub use mock_class_manager::MockClassManager as ContractClassManager;
