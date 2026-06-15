struct Inner { int v; };
struct Outer { struct Inner i; };
struct Outer o;
int main(void) {
  o.i.v = 42;
  return o.i.v;
}
