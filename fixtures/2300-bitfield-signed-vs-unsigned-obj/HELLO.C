struct S { signed int s : 4; unsigned int u : 4; };
int main(void) {
  struct S x;
  x.s = -1;
  x.u = 15;
  return x.s + (int)x.u;
}
