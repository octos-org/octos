"""Streaming transport for independent review, with bounded transient retries.

No acceptance policy lives here. Partial streams never become completed reviews;
valid negative judgments are returned once, never retried to seek a passing one.
"""
import copy
import json
import time

import requests


class IncompleteStream(RuntimeError):
    pass


class ReviewProtocolError(RuntimeError):
    """The checker produced no usable judgment; this is not an artifact defect."""
    pass


def validated_review(generate, validate, *, attempts=3, progress=None):
    """Retry invalid report syntax/schema only. Never repair or retry a valid fail.

    generate(attempt) must preserve each raw response in host-owned evidence.
    Exhausted schema/coverage failures remain explicit checker errors.
    """
    if attempts < 1:
        raise ValueError('Review attempts must be positive')
    for attempt in range(1, attempts + 1):
        data = generate(attempt)
        try:
            choice = data['choices'][0]
            if choice.get('finish_reason') != 'stop':
                raise ReviewProtocolError('Reviewer output did not finish; no judgment accepted')
            answer = choice['message']['content']
        except (KeyError, IndexError, TypeError) as error:
            raise ReviewProtocolError('Invalid reviewer response envelope') from error
        try:
            return validate(answer), data
        except json.JSONDecodeError as error:
            if progress:
                progress({'event': 'invalid_review_json', 'attempt': attempt,
                          'line': error.lineno, 'column': error.colno})
            if attempt == attempts:
                raise ReviewProtocolError('Reviewer returned invalid JSON after format retries; '
                                          'no artifact judgment was produced') from error
        except (ValueError, TypeError, KeyError, AttributeError) as error:
            if progress:
                progress({'event': 'invalid_review_schema', 'attempt': attempt,
                          'error_type': type(error).__name__, 'detail': str(error)[:240]})
            if attempt == attempts:
                raise ReviewProtocolError('Reviewer evidence schema is invalid after format retries: ' + str(error)) from error


def collect_stream(lines, progress=None):
    text, reasoning, usage = [], [], None
    model = ident = finish = None
    done = False
    events = 0
    last_progress = 0
    for raw in lines:
        line = raw.decode('utf-8') if isinstance(raw, bytes) else raw
        if not line or not line.startswith('data:'):
            continue
        payload = line[5:].strip()
        if payload == '[DONE]':
            done = True
            break
        event = json.loads(payload)
        if event.get('error'):
            raise RuntimeError('Review provider returned a stream error')
        model = event.get('model') or model
        ident = event.get('id') or ident
        usage = event.get('usage') or usage
        for choice in event.get('choices', []):
            if choice.get('index', 0) != 0:
                raise ValueError('Reviewer transport expects one completion')
            delta = choice.get('delta', {})
            if delta.get('content'):
                if not isinstance(delta['content'], str):
                    raise ValueError('Unexpected review content type')
                text.append(delta['content'])
            if delta.get('reasoning_content'):
                reasoning.append(delta['reasoning_content'])
            finish = choice.get('finish_reason') or finish
        events += 1
        if progress and time.monotonic() - last_progress >= 20:
            progress({'event': 'stream_active', 'events': events})
            last_progress = time.monotonic()
    if not done or not finish or not ''.join(text).strip():
        raise IncompleteStream('Review stream ended without complete content and terminal markers')
    message = {'role': 'assistant', 'content': ''.join(text)}
    if reasoning:
        message['reasoning_content'] = ''.join(reasoning)
    return {'id': ident, 'model': model, 'choices': [{'index': 0, 'finish_reason': finish, 'message': message}], 'usage': usage}


def chat_completion(url, headers, payload, *, attempts=3, timeout=(20, 600),
                    post=None, sleep=time.sleep, progress=None):
    if attempts < 1:
        raise ValueError('Transport attempts must be positive')
    request = copy.deepcopy(payload)
    request['stream'] = True
    request['stream_options'] = {**request.get('stream_options', {}), 'include_usage': True}
    if request.get('n', 1) != 1:
        raise ValueError('Reviewer transport expects one completion')
    send = post or requests.post
    for attempt in range(1, attempts + 1):
        if progress:
            progress({'event': 'attempt_started', 'attempt': attempt})
        try:
            with send(url, headers=headers, json=request, stream=True, timeout=timeout) as response:
                response.raise_for_status()
                result = collect_stream(response.iter_lines(), progress)
            if progress:
                progress({'event': 'transport_complete', 'attempt': attempt})
            return result
        except (requests.RequestException, IncompleteStream) as error:
            status = getattr(getattr(error, 'response', None), 'status_code', None)
            transient = (isinstance(error, (requests.Timeout, requests.ConnectionError,
                                            requests.exceptions.ChunkedEncodingError, IncompleteStream))
                         or status in {429, 500, 502, 503, 504})
            if not transient or attempt == attempts:
                raise
            delay = min(2 ** (attempt - 1), 10)
            retry_after = getattr(getattr(error, 'response', None), 'headers', {}).get('Retry-After')
            if retry_after:
                try:
                    delay = max(delay, min(float(retry_after), 30))
                except ValueError:
                    pass
            if progress:
                progress({'event': 'transport_retry', 'attempt': attempt, 'http_status': status,
                          'error_type': type(error).__name__, 'delay_seconds': delay})
            sleep(delay)
