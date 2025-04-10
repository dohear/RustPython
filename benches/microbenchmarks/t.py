import dis as d
def foo():
    a = 10
    if a == 10:
        a = a + 1
    else:
        a = a + 10
    return a 


d.dis(foo)
foo.__jit__()
print(foo())
