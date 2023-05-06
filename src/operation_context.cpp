#if 0
// Utilities for io redirection.
#include "config.h"  // IWYU pragma: keep

#include "operation_context.h"

#include <utility>

#include "env.h"

bool no_cancel() { return false; }

operation_context_t::operation_context_t(ParserRef parser,
                                         const environment_t &vars, cancel_checker_t cancel_checker,
                                         size_t expansion_limit)
    : parser(std::move(parser)),
      vars(vars),
      expansion_limit(expansion_limit),
      cancel_checker(std::move(cancel_checker)) {}

operation_context_t operation_context_t::empty() {
    static const null_environment_t nullenv{};
    return operation_context_t{nullenv};
}

operation_context_t operation_context_globals() {
    return operation_context_t{env_stack_globals()};
}

operation_context_t::~operation_context_t() = default;
#endif
