#!/usr/bin/python
from gui import *
import time

def send(timest, nick, msg):
    print(timest, nick, msg)
    node_id = api.lookup_node_id("/window/view/chatty")

    arg_data = bytearray()
    serial.write_u64(arg_data, timest)
    arg_data += bytes(32)
    serial.encode_str(arg_data, nick)
    serial.encode_str(arg_data, msg)

    api.call_method(node_id, "insert_line", arg_data)

for i in range(27):
    name = f"bob-{i}"
    send(1722944640000 + i*60000, "hhi12", f"hello {name}")
    #time.sleep(0.4)
    input("> ")

