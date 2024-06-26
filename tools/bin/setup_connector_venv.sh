#!/usr/bin/env bash

set -e

CONNECTOR=$1
PYTHON_EXECUTABLE=${2-python}

cd airbyte-integrations/connectors/$CONNECTOR
if [[ ! -d .venv ]]; then
    echo Installing requirements for $CONNECTOR
    $PYTHON_EXECUTABLE -m venv --without-pip .venv
    source .venv/bin/activate
    python ../../../get-pip.py
    pip install -r requirements.txt
    pip install '.[tests]'
    pip install pytest-cov
else
    echo "directory .venv exists, skipping"
fi