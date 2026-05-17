struct S { unsigned char b; int x; };
struct S s;
int g;
int main(void) {
  s.b = 200;
  g = s.b + 1;
  return 0;
}
