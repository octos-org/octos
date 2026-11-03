import copy
import json
from pathlib import Path
import sys
import unittest
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import review_transport as transport
import requests


def event(delta=None, finish=None, **extra):
    return ('data: ' + json.dumps({'id': 'test', 'model': 'configured-model',
            'choices': [{'index': 0, 'delta': delta or {}, 'finish_reason': finish}], **extra})).encode()


class Response:
    def __init__(self, lines, status=200):
        self.lines = lines; self.status_code = status; self.headers = {}
    def __enter__(self): return self
    def __exit__(self, *_): pass
    def raise_for_status(self):
        if self.status_code != 200:
            raise requests.HTTPError('HTTP ' + str(self.status_code), response=self)
    def iter_lines(self): return iter(self.lines)


class TransportTests(unittest.TestCase):
    def complete(self, text='{"status":"fail"}', finish='stop'):
        return [event({'reasoning_content': 'private reasoning'}), event({'content': text}),
                event(finish=finish), b'data: {"choices":[],"usage":{"prompt_tokens":123}}', b'data: [DONE]']

    def test_complete_negative_verdict_returned_without_retry(self):
        calls = []; events = []
        def post(*a, **kw):
            calls.append(kw); return Response(self.complete())
        payload = {'model': 'configured-model', 'messages': [{'role': 'user', 'content': 'original'}]}
        before = copy.deepcopy(payload)
        result = transport.chat_completion('unused', {}, payload, post=post, progress=events.append)
        self.assertEqual(result['choices'][0]['message']['content'], '{"status":"fail"}')
        self.assertEqual(result['usage']['prompt_tokens'], 123)
        self.assertEqual(len(calls), 1); self.assertEqual(payload, before)
        self.assertTrue(calls[0]['stream']); self.assertTrue(calls[0]['json']['stream'])
        self.assertNotIn('private reasoning', json.dumps(events))

    def test_transient_504_retries_same_request(self):
        calls = []; sleeps = []
        def post(*a, **kw):
            calls.append(copy.deepcopy(kw))
            return Response([], 504) if len(calls) == 1 else Response(self.complete())
        transport.chat_completion('unused', {}, {'model': 'test'}, post=post, sleep=sleeps.append)
        self.assertEqual(calls[0], calls[1]); self.assertEqual(sleeps, [1])

    def test_truncated_stream_does_not_pass_even_with_valid_json(self):
        calls = []
        def post(*a, **kw):
            calls.append(1); return Response([event({'content': '{"status":"pass"}'})])
        with self.assertRaises(transport.IncompleteStream):
            transport.chat_completion('unused', {}, {}, post=post, sleep=lambda _: None)
        self.assertEqual(len(calls), 3)

    def test_auth_error_not_retried(self):
        calls = []
        def post(*a, **kw): calls.append(1); return Response([], 401)
        with self.assertRaises(requests.HTTPError):
            transport.chat_completion('unused', {}, {}, post=post)
        self.assertEqual(len(calls), 1)

    def test_output_limit_not_silently_treated_as_finished_or_retried(self):
        result = transport.collect_stream(self.complete(finish='length'))
        self.assertEqual(result['choices'][0]['finish_reason'], 'length')

    def test_malformed_sse_does_not_pass(self):
        with self.assertRaises(ValueError):
            transport.collect_stream([b'data: broken json', b'data: [DONE]'])

    def test_disconnect_retries_without_mixing_partial_content(self):
        calls = []
        def disconnected():
            yield event({'content': 'partial'})
            raise requests.ConnectionError('lost connection')
        def post(*a, **kw):
            calls.append(1)
            return Response(disconnected()) if len(calls) == 1 else Response(self.complete('final'))
        result = transport.chat_completion('unused', {}, {}, post=post, sleep=lambda _: None)
        self.assertEqual(result['choices'][0]['message']['content'], 'final')


class ReviewProtocolTests(unittest.TestCase):
    def response(self, text, finish='stop'):
        return {'choices': [{'finish_reason': finish, 'message': {'content': text}}]}

    def test_bad_json_retries_and_keeps_valid_negative(self):
        generate = Mock(side_effect=[self.response('{"bbox":[0:0.7]}'),
                                    self.response('{"status":"fail"}')])
        events = []
        value, _ = transport.validated_review(generate, json.loads, progress=events.append)
        self.assertEqual(value['status'], 'fail')
        self.assertEqual(generate.call_count, 2)
        self.assertEqual(events[0]['event'], 'invalid_review_json')

    def test_valid_negative_or_unknown_never_retried(self):
        for status in ['fail', 'unknown']:
            generate = Mock(return_value=self.response(json.dumps({'status': status})))
            value, _ = transport.validated_review(generate, json.loads)
            self.assertEqual(value['status'], status)
            self.assertEqual(generate.call_count, 1)

    def test_syntax_exhaustion_never_becomes_acceptance(self):
        generate = Mock(return_value=self.response('{"status":"pass",}'))
        with self.assertRaises(transport.ReviewProtocolError):
            transport.validated_review(generate, json.loads)
        self.assertEqual(generate.call_count, 3)

    def test_missing_coverage_cannot_pass_after_bounded_schema_retries(self):
        generate = Mock(return_value=self.response('{"objects":[]}'))
        def validate(answer):
            json.loads(answer)
            raise ValueError('Incomplete native-object coverage')
        with self.assertRaises(transport.ReviewProtocolError):
            transport.validated_review(generate, validate)
        self.assertEqual(generate.call_count, 3)

    def test_invalid_schema_retries_but_valid_fail_stops(self):
        generate = Mock(side_effect=[self.response('{"objects":[]}'),self.response('{"status":"fail"}')])
        def validate(answer):
            value=json.loads(answer)
            if 'status' not in value:raise ValueError('Invalid independently observed owner region')
            return value
        value, _=transport.validated_review(generate,validate)
        self.assertEqual(value['status'],'fail')
        self.assertEqual(generate.call_count,2)

    def test_truncation_cannot_be_validated(self):
        generate = Mock(return_value=self.response('{"status":"pass"}', finish='length'))
        validate = Mock(return_value={'status': 'pass'})
        with self.assertRaises(transport.ReviewProtocolError):
            transport.validated_review(generate, validate)
        validate.assert_not_called()


if __name__ == '__main__': unittest.main()
