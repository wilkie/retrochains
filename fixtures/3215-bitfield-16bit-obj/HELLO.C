struct B { unsigned int x : 4; unsigned int y : 4; unsigned int z : 8; };
struct B b;
int sz(void) {
  return sizeof(b);
}
