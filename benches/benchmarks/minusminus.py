def foo(a: int, b: int):
    return a-(-b)

a = 1
b = 10000
try:
    foo.__jit__()
except:
    pass
while a < b:
    a = foo(a, b)
    b -= 1
    