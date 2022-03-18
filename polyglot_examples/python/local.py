#!/usr/bin/env python3

import secrets
import hashlib
import requests
import threading

mailbox_url = "http://localhost:8000"

mailbox = secrets.token_bytes(32)
address = hashlib.sha256(mailbox).digest()

threading.Thread(
    target=lambda: requests.post(
        mailbox_url + "/hook/" + address.hex(),
        json={"message": "hey!"},
    )
).start()

result = requests.get(
    mailbox_url + "/watch",
    headers={"Authorization": "Bearer " + mailbox.hex()},
)
assert result.text == '{"message": "hey!"}'
