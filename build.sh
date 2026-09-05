#!/bin/sh

docker build --target release-bundle --output type=local,dest=.bundle .
