struct S { char c; };
struct S s = {'Q'};
int main(void) {
  char b = s.c;
  return b;
}
