/* Compile-only ABI declaration check; no provider/runtime calls. */
#include "octos.h"

void octos_header_contract(void) {
    char *(*run)(OctosRuntime *, const char *) = octos_run_task;
    char *(*take_partial)(void) = octos_take_last_partial_result;
    const char *(*diagnostic)(void) = octos_last_error;
    void (*release)(char *) = octos_string_free;
    char *(*memory_upsert)(OctosRuntime *, const char *) = octos_memory_upsert;
    char *(*memory_search)(OctosRuntime *, const char *) = octos_memory_search;
    char *(*memory_load)(OctosRuntime *, const char *) = octos_memory_load;
    char *(*memory_stats)(OctosRuntime *) = octos_memory_stats;
    char *(*model_status)(const char *) = octos_embedding_model_status;
    char *(*model_ensure)(const char *, bool) = octos_embedding_model_ensure;
    (void)run;
    (void)take_partial;
    (void)diagnostic;
    (void)release;
    (void)memory_upsert;
    (void)memory_search;
    (void)memory_load;
    (void)memory_stats;
    (void)model_status;
    (void)model_ensure;
}
