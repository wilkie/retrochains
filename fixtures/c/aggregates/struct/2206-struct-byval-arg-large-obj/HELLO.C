struct Big { int a; int b; int c; int d; };
int sum_big(struct Big b) {
  return b.a + b.b + b.c + b.d;
}
int main(void) {
  struct Big bg;
  bg.a = 1; bg.b = 2; bg.c = 3; bg.d = 4;
  return sum_big(bg);
}
