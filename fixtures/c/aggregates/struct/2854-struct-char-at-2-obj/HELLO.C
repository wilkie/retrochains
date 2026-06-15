struct R { int a; char b; };
struct R r;
int main(void) {
  r.a = 1;
  r.b = 'Z';
  return r.b;
}
