# Websocket Schema
This doccument describes interactions between the speech-to-text client and the backend.

## Communication
Using json over websockets to communicate between the client and the backend.

## Schema
All messages related to actions done by users will include the following fields:
```jsonc
{
    "user": {
        "id": string, // discord id
        "username": string, // discord username
    },
    "timestamp": string, // ISO 8601 timestamp
    "type": string, // type of message being sent
}
```
The above fields (except for `type`) are not included in the examples below for brevity.

### Understood Speech
Send when a user's speech is understood by the speech-to-text engine.
```jsonc
{
    "type": "UnderstoodSpeech",
    "started_speaking_at": string, // the ISO 8601 timestamp that this user started speaking
    "speech_duration": float, // the duration of the speech in seconds
    "speech_text": string, // the text that the ai thought the person said
    "no_speech_prob": float, // the probablility that nothing was said, ranging from zero to one
}
```

### Join Connect
Sent when a user joins a voice channel.
```jsonc
{
    "type": "Connect"
}
```

### First Time Speaking
Sent when a user speaks for the first time after joining a channel.
```jsonc
{
    "type": "FirstTimeSpeaking"
}
```

### Speaking Begin
Sent when a user starts speaking.
```jsonc
{
    "type": "SpeakingBegin"
}
```

### Speaking End
Sent when a user stops speaking.
```jsonc
{
    "type": "SpeakingEnd"
}
```

### User Disconnect
Sent when a user disconnects from the voice channel.
```jsonc
{
    "type": "UserDisconnected"
}
```