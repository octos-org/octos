"""Transport fault injection; no model calls or artifact-quality claims."""
import importlib.util
from pathlib import Path
import socket
import unittest

spec=importlib.util.spec_from_file_location('oup_reconnect',Path(__file__).resolve().parents[1]/'scripts/oup_reconnect.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)

class Sock:
    def close(self): pass
    def settimeout(self, value): pass

class ReconnectTests(unittest.TestCase):
    def fixture(self):
        self.calls=[];self.created=[];self.notes=[];self.alive=True;self.native_state='interrupted'
        outer=self
        class Raw:
            def __init__(self):
                self.sock=Sock();self.buffer=bytearray();self.pieces=[];self.events=[];self.next_id=0
                self.fail_receive=None;self.fail_rpc=None;self.reject=False
                outer.created.append(self)
            def receive(self):
                if self.fail_receive:raise self.fail_receive
                raise socket.timeout()
            def rpc(self,method,params):
                outer.calls.append((method,params));self.next_id+=1
                if self.fail_rpc==method:
                    self.fail_rpc=None;raise RuntimeError('WebSocket closed')
                if method=='session/open':
                    if outer.created[0].reject:raise RuntimeError('session/open failed: forbidden')
                    if len(outer.created)>1:
                        self.events.append({'method':'projection/envelope','params':{'cursor':{'stream':'scope','seq':11},'payload':{'type':'turn_terminal','data':{'outcome':'interrupted'}}}})
                    return {'opened':{'session_id':params['session_id']}}
                if method=='turn/state/get':return {'state':outer.native_state}
                return {}
        cls=m.reconnecting_client(Raw,lambda:self.alive,lambda kind,**kw:self.notes.append((kind,kw)),sleep=lambda _:None)
        c=cls();c.rpc('session/open',{'session_id':'bound','profile_id':'profile','cwd':'/workspace'})
        return c

    def test_eof_reopens_same_binding_after_cursor_and_retains_native_interruption(self):
        c=self.fixture();c.cursor={'stream':'scope','seq':10};c.fail_receive=RuntimeError('WebSocket closed')
        result=c.receive()
        opens=[p for method,p in self.calls if method=='session/open']
        self.assertEqual(opens[-1],{'session_id':'bound','profile_id':'profile','cwd':'/workspace','after':{'stream':'scope','seq':10}})
        self.assertEqual(result['params']['payload']['data']['outcome'],'interrupted')
        self.assertFalse(any(method in {'turn/start','permission/profile/set'} for method,_ in self.calls))
        self.assertEqual(self.notes[-1][0],'transport_reconnected')

    def test_lost_start_ack_queries_exact_turn_without_resending(self):
        c=self.fixture();c.fail_rpc='turn/start'
        r=c.rpc('turn/start',{'session_id':'bound','turn_id':'original','input':[]})
        self.assertTrue(r['accepted'])
        self.assertEqual(sum(method=='turn/start' for method,_ in self.calls),1)
        self.assertIn(('turn/state/get',{'session_id':'bound','turn_id':'original'}),self.calls)

    def test_unknown_start_is_ambiguous_and_never_resent(self):
        c=self.fixture();c.fail_rpc='turn/start';self.native_state='unknown'
        with self.assertRaises(m.AmbiguousCommand):c.rpc('turn/start',{'session_id':'bound','turn_id':'original'})
        self.assertEqual(sum(method=='turn/start' for method,_ in self.calls),1)

    def test_lost_mutating_command_does_not_get_automatically_replayed(self):
        c=self.fixture();c.fail_rpc='turn/steer'
        with self.assertRaises(m.AmbiguousCommand):c.rpc('turn/steer',{'session_id':'bound'})
        self.assertEqual(sum(method=='turn/steer' for method,_ in self.calls),1)

    def test_idle_timeout_does_not_reconnect(self):
        c=self.fixture()
        with self.assertRaises(socket.timeout):c.receive()
        self.assertEqual(len(self.created),1)

    def test_dead_host_ends_bounded_reconnect_without_new_command(self):
        c=self.fixture();self.alive=False;c.fail_receive=RuntimeError('WebSocket closed')
        with self.assertRaises(m.HostUnavailable):c.receive()
        self.assertEqual(len(self.created),1)

    def test_auth_rejection_is_preserved_without_new_credentials_or_permissions(self):
        c=self.fixture();c.reject=True;c.fail_receive=RuntimeError('WebSocket closed')
        with self.assertRaisesRegex(RuntimeError,'forbidden'):c.receive()
        self.assertEqual(len(self.created),2)
        self.assertFalse(any(method=='permission/profile/set' for method,_ in self.calls))

if __name__=='__main__':unittest.main()
