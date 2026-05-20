int main(void) {
  int x = 7;
  int huge *p = (int huge *)&x;
  return *p;
}
