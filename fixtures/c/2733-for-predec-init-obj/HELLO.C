int main(void) {
  int i;
  int s;
  s = 0;
  for (i = 5; --i; ) {
    s = s + 1;
  }
  return s;
}
