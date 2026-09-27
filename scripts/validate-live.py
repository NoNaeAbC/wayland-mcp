#!/usr/bin/env python3
"""Visible GTK4 integration check through the MCP on the current desktop."""
import json
import os
from pathlib import Path
import queue
import runpy
import subprocess
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
processes = []
logs = []

def spawn(argv, env, directory, name, *, input_pipe=False, output_pipe=False):
    log = open(directory / (name + '.log'), 'w')
    logs.append(log)
    process = subprocess.Popen(argv, env=env, stdin=subprocess.PIPE if input_pipe else subprocess.DEVNULL,
                               stdout=subprocess.PIPE if output_pipe else log, stderr=log, text=True, bufsize=1)
    processes.append(process)
    return process

def wait_until(check, description, seconds=15):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        time.sleep(.05)
    raise RuntimeError('timed out: ' + description)

class Mcp:
    def __init__(self, process):
        self.process = process
        self.responses = queue.Queue()
        self.serial = 0
        threading.Thread(target=self.read, daemon=True).start()
    def read(self):
        for line in self.process.stdout:
            self.responses.put(json.loads(line))
    def call(self, method, params):
        self.serial += 1
        self.process.stdin.write(json.dumps(dict(jsonrpc='2.0', id=self.serial, method=method, params=params)) + '\n')
        self.process.stdin.flush()
        while True:
            response = self.responses.get(timeout=30)
            if response.get('id') == self.serial:
                if 'error' in response:
                    raise RuntimeError(response['error'])
                return response['result']
    def js(self, code):
        result = self.call('tools/call', dict(name='gui_console', arguments=dict(code=code)))
        if result.get('isError'):
            raise RuntimeError(result)
        return result['structuredContent']['value']

if __name__ == '__main__':
    runpy.run_path(str(Path(__file__).with_name('demonstrate-live.py')), run_name='__main__')
