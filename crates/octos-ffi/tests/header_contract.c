/* Compile-only ABI declaration check; no provider/runtime calls. */
#include "octos.h"

void octos_header_contract(void) {
    char *(*run)(OctosRuntime *, const char *) = octos_run_task;
    char *(*take_partial)(void) = octos_take_last_partial_result;
    const char *(*diagnostic)(void) = octos_last_error;
    void (*release)(char *) = octos_string_free;
    (void)run;
    (void)take_partial;
    (void)diagnostic;
    (void)release;
}
