// clef-token-count: exact Clef System One input token counts, without inference.
//
// Loads only the vocabulary and metadata of a Clef GGUF and runs the same
// parse_questions -> parse_state -> fill_task_joint path as the pinned
// llama-server, so counts match `usage.input_tokens` of /v1/systemone.
//
// Protocol (one JSON object per line):
//   startup, stdout: {"ready":true,"decision_type":"clef","llama_cpp_commit":"..."}
//   stdin:           {"id":"<any>","body":{<systemone request body>}}
//   stdout:          {"id":"<any>","input_tokens":N}
//                or  {"id":"<any>","error":{"kind":"invalid_request"|"internal","message":"..."}}
// Logs go to stderr. EOF on stdin exits with status 0.

#include "server-decision.h"

#include "common.h"
#include "llama.h"
#include "log.h"

#include <cstdio>
#include <exception>
#include <iostream>
#include <stdexcept>
#include <string>

#ifndef CLEF_TOKEN_COUNT_LLAMA_COMMIT
#define CLEF_TOKEN_COUNT_LLAMA_COMMIT "unknown"
#endif

static void emit(const json & value) {
    // one line per message, flushed so the miner can read it immediately
    std::cout << value.dump_safe() << '\n' << std::flush;
}

static json error_reply(const json & id, const char * kind, const std::string & message) {
    return json{{"id", id}, {"error", json{{"kind", kind}, {"message", message}}}};
}

int main(int argc, char ** argv) {
    if (argc != 2 || std::string(argv[1]) == "--help") {
        std::fprintf(stderr, "usage: %s <clef-model.gguf>\n", argv[0]);
        return 2;
    }
    if (std::string(argv[1]) == "--version") {
        std::printf("clef-token-count llama.cpp:%s\n", CLEF_TOKEN_COUNT_LLAMA_COMMIT);
        return 0;
    }

    llama_backend_init();

    llama_model_params params = llama_model_default_params();
    params.vocab_only = true;
    llama_model * model = llama_model_load_from_file(argv[1], params);
    if (model == nullptr) {
        std::fprintf(stderr, "failed to load vocabulary from %s\n", argv[1]);
        return 1;
    }

    server_decision_context decision;
    try {
        decision.init(model);
    } catch (const std::exception & e) {
        std::fprintf(stderr, "not a usable decision model: %s\n", e.what());
        return 1;
    }
    if (!decision.is_joint()) {
        std::fprintf(stderr, "model is not a joint (clef) decision model\n");
        return 1;
    }

    emit(json{{"ready", true}, {"decision_type", "clef"}, {"llama_cpp_commit", CLEF_TOKEN_COUNT_LLAMA_COMMIT}});

    std::string line;
    while (std::getline(std::cin, line)) {
        if (line.empty()) {
            continue;
        }
        json id = nullptr;
        try {
            const json request = json::parse(line);
            if (request.contains("id")) {
                id = request.at("id");
            }
            if (!request.contains("body") || !request.at("body").is_object()) {
                emit(error_reply(id, "invalid_request", "\"body\" must be an object"));
                continue;
            }
            const json & body = request.at("body");

            const auto questions = decision.parse_questions(body);
            std::vector<raw_buffer> files;
            const json state = decision.parse_state(body, files);
            if (!files.empty()) {
                emit(error_reply(id, "invalid_request", "image input is not supported for clef"));
                continue;
            }

            server_task task(SERVER_TASK_TYPE_DECISION);
            decision.fill_task_joint(state, questions, task);
            emit(json{{"id", id}, {"input_tokens", task.tokens.size()}});
        } catch (const std::invalid_argument & e) {
            emit(error_reply(id, "invalid_request", e.what()));
        } catch (const common_json_error & e) {
            emit(error_reply(id, "invalid_request", std::string("invalid JSON: ") + e.what()));
        } catch (const std::exception & e) {
            emit(error_reply(id, "internal", e.what()));
        }
    }

    llama_model_free(model);
    llama_backend_free();
    return 0;
}
