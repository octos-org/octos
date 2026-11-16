"""Reconnect an owning OUP client without replaying ambiguous mutations.

This adapter uses normal authenticated session/open and turn/state/get. A closed
connection may interrupt a connection-bound turn; native replay stays authoritative.
It never changes permissions, invents a terminal event, or restarts an agent turn.
"""
import socket
import time


class HostUnavailable(RuntimeError):
    pass


class AmbiguousCommand(RuntimeError):
    pass


def reconnecting_client(raw_client, healthy, notify, sleep=time.sleep):
    class Client(raw_client):
        def __init__(self):
            self.binding = None
            self.cursor = None
            self._in_rpc = False
            self._reopened = None
            super().__init__()

        @staticmethod
        def _transport_error(exc):
            return not isinstance(exc, socket.timeout) and (
                isinstance(exc, OSError)
                or str(exc).startswith(('WebSocket closed', 'Incomplete WebSocket handshake'))
            )

        def _observe(self, message):
            cursor = message.get('params', {}).get('cursor')
            if cursor and ('seq' in cursor) and (
                self.cursor is None or
                cursor.get('stream') == self.cursor.get('stream') and cursor['seq'] > self.cursor['seq']
            ):
                self.cursor = dict(cursor)
            return message

        def _reconnect(self):
            if not self.binding:
                raise HostUnavailable('Cannot reconnect an unbound OUP client')
            notify('transport_reconnecting', session_id=self.binding['session_id'])
            last = None
            for delay in (0, 0.5, 1, 2):
                if delay:
                    sleep(delay)
                if not healthy():
                    last = HostUnavailable('Native host is not healthy; no command was resent')
                    continue
                replacement = None
                try:
                    replacement = raw_client()
                    params = dict(self.binding)
                    if self.cursor:
                        params['after'] = dict(self.cursor)
                    opened = replacement.rpc('session/open', params)
                    # Read the existing policy; never widen it during reconnect.
                    replacement.rpc('session/status/read', {'session_id': params['session_id']})
                    old_socket = self.sock
                    pending = list(self.events)
                    pending.extend(replacement.events)
                    old_next_id = self.next_id
                    for name in ('sock', 'buffer', 'pieces', 'next_id'):
                        setattr(self, name, getattr(replacement, name))
                    self.next_id = max(old_next_id, self.next_id)
                    self.events = pending
                    self.sock.settimeout(1)
                    old_socket.close()
                    self._reopened = opened
                    notify('transport_reconnected', session_id=params['session_id'], after=self.cursor)
                    return
                except Exception as exc:
                    if replacement:
                        replacement.sock.close()
                    if not self._transport_error(exc):
                        raise  # Authentication/policy/RPC rejection is never bypassed.
                    last = exc
            raise HostUnavailable(str(last))

        def receive(self):
            while True:
                try:
                    return self._observe(super().receive())
                except Exception as exc:
                    if not self._transport_error(exc) or self._in_rpc:
                        raise
                    self._reconnect()
                    if self.events:
                        return self._observe(self.events.pop(0))

        def rpc(self, method, params):
            if method == 'session/open':
                self.binding = {k: v for k, v in params.items() if k != 'after'}
            self._in_rpc = True
            try:
                return super().rpc(method, params)
            except Exception as exc:
                if not self._transport_error(exc):
                    raise
                self._reconnect()
                if method == 'session/open':
                    return self._reopened
                if method in {'session/status/read', 'turn/state/get', 'tool/status/list'}:
                    return super().rpc(method, params)
                if method == 'turn/start':
                    state = super().rpc('turn/state/get', {
                        'session_id': params['session_id'], 'turn_id': params['turn_id']})
                    if state.get('state') in {'active','interrupting','completed','errored','interrupted'}:
                        return {'accepted': True, 'reconciled_after_disconnect': True}
                raise AmbiguousCommand(f'{method}: response lost; mutation was not resent') from exc
            finally:
                self._in_rpc = False

    return Client
