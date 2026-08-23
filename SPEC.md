# Doorbell Webhook

## Introduction

The aim here is to build a small binary that does two things:

1. A webhook server
2. A Chromecast client

This binary essentially receives HTTP post requests, parses and validates the payload.

The payload information is then used to send a request to one or multiple Chromecast devices.

## Webhook Server

The webhook server should be explict about what endpoints is accepts. Each endpoint is specific by the user via a command line flag.
Endpoints should be at least 8 characters long. Invalid or not specified endpoints return a 404.

HTTP requests along with their payload should always be logged.

## Chromecast client

Each endpoint on the webhook server must match at least one chromecast device address - this is also done via a command line flag.
Parameters from the webhook payload will be used to make a request to the chromecast devices to perform these actions:

1. play a doorbell ring sound on the chromecast device(s)
2. display the image from the payload on the chromecast device(s)
3. play a live video stream (external to the payload, hence specified via a parameter in the cli) on the chromecast device(s)

These requests are configurable by chromecast device. E.g. not every device will do all the 3 actions defined above. This is also defined via the cli.

## CLI

The cli should allow:

1. defining the address of the chromecast devices and an alias for each device
2. defining a list of actions, where each action is paired up with a device alias
3. setting external values to be used in a payload, such as a livestream url or a regular url with media


The CLI will run the webhook server and should be a written in Rust. It should use a modern, async chromecast client that supports the functionality required to implement the actions above.
