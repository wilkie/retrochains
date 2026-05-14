union u { int i; char c; };
int main(void) {
  union u v;
  v.i = 300;
  return v.c;
}
