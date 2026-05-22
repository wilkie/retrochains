struct Inner { int v; };
struct Outer { struct Inner i; int z; };
struct Outer obj;
int main(void) {
  obj.i.v = 7;
  return obj.i.v;
}
