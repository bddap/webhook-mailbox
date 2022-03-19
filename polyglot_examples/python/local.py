#!/usr/bin/env python3

import secrets
import hashlib
import requests
import threading
import argparse

parser = argparse.ArgumentParser(description="webhook-mailbox client example")
parser.add_argument(
    "-m",
    "--mailbox-url",
    type=str,
    default="https://webhook-mailbox.com/api",
    help="the base url of the target mailbox, e.g. http://localhost:8000",
)
args = parser.parse_args()

mailbox_url = args.mailbox_url

mailbox = secrets.token_bytes(32)
address = hashlib.sha256(mailbox).digest()


def sender():
    result = requests.post(
        mailbox_url + "/hook/" + address.hex(),
        json={"message": "hey!"},
    )
    print("sender got:", result)


threading.Thread(target=sender).start()

result = requests.get(
    mailbox_url + "/watch",
    headers={"Authorization": "Bearer " + mailbox.hex()},
)
print("watcher got:", result)
print("watcher body:", result.text)
assert result.status_code == 200
assert result.text == '{"message": "hey!"}'

print("It worked!")
