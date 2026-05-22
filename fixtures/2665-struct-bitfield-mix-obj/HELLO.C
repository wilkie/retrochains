struct S {
  unsigned flag : 1;
  unsigned kind : 7;
  int value;
};
struct S s;
int main(void) {
  s.flag = 1;
  s.value = 42;
  return s.value;
}
