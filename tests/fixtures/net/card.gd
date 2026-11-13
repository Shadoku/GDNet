extends Node2D
## Test fixture (WP16): a card whose face only its holder learns. The scene's `NetEntity` marks `face`
## `audience: owner`; every peer has the card, the others keep the scene's -1.

## The card's face, set on the server.
@export var face: int = -1

## A name only the holder learns (`audience: owner`, a string so a test can find it in packets).
@export var secret_name: String = ""
