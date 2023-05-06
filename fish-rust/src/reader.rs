use crate::env::{EnvStackRefFFI, EnvironmentRef, EnvironmentRefFFI};
use crate::operation_context::OperationContext;

pub fn reader_schedule_prompt_repaint() {
    todo!()
}

/// \return whether fish is currently unwinding the stack in preparation to exit.
pub fn fish_is_unwinding_for_exit() -> bool {
    todo!()
}

pub fn restore_term_mode() {
    todo!()
}

pub fn reader_run_count() -> u64 {
    todo!()
}

/// \return an operation context for a background operation..
/// Crucially the operation context itself does not contain a parser.
/// It is the caller's responsibility to ensure the environment lives as long as the result.
fn get_bg_context(env: &EnvironmentRef, generation_count: u32) -> OperationContext<'static> {
    todo!()
    // const std::shared_ptr<environment_t> &env,
    //                                       uint32_t generation_count) {
    // cancel_checker_t cancel_checker = [generation_count] {
    //     // Cancel if the generation count changed.
    //     return generation_count != read_generation_count();
    // };
    // return operation_context_t{nullptr, *env, std::move(cancel_checker), kExpansionLimitBackground};
}

fn get_bg_context_ffi(
    env: &EnvironmentRefFFI,
    generation_count: u32,
) -> Box<OperationContext<'static>> {
    Box::new(get_bg_context(&env.0, generation_count))
}

#[cxx::bridge]
mod reader_ffi {
    extern "C++" {
        include!("operation_context.h");
        include!("env.h");
        #[cxx_name = "EnvironmentRef"]
        type EnvironmentRefFFI = crate::env::EnvironmentRefFFI;
        type OperationContext<'a> = crate::operation_context::OperationContext<'a>;
    }
    extern "Rust" {
        // #[cxx_name = "get_bg_context"]
        // fn get_bg_context_ffi(
        //     env: &EnvironmentRefFFI,
        //     generation_count: u32,
        // ) -> Box<StaticOperationContext>;
        // ) -> Box<OperationContext<'static>>;
    }
}
