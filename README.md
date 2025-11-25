# Lake lang frontend

```
person is
	p: person -> print [ p.eye_color ]
	@T{ eye_color: &[u8] } -> T

factorial is
	n: int acc: int.1 -> factorial [ n acc ]
	n: int acc: int -> when n is
		0 -> int.1
		n -> factorial [ n-1 acc*n ] 

main is
	x: i32. 10 -> when x is
		10 -> print [ "x is ten" ]
		20 -> print [ "x is twenty" ]
	s: string. "hello" -> print [ s ]
	p: person. { eye_color: &[255 255 255] } -> person [ p2 ]
	n: i32. 10 -> factorial [ n ]

coop main | person [ person.{ eye_color: &[] } ]
```
