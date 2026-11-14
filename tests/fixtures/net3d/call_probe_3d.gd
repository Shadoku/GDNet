extends Node3D
## Test fixture (WP18): a 3D entity whose methods take 3D values, recording every call on this peer.
## Nothing here is declared for the network: every public method is callable.

## Every execution on this peer: [method, argument...].
var calls: Array = []


func place(at: Vector3, turn: Quaternion, basis_value: Basis, xform: Transform3D) -> void:
	calls.append(["place", at, turn, basis_value, xform])
	transform = xform


func mirrored(xform: Transform3D) -> Transform3D:
	calls.append(["mirrored", xform])
	return xform.rotated(Vector3.UP, PI)
