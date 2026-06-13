int five(void) {
  return 5;
}
int main(void) {
  int x = 0;
  int *p = &x;
  *p = five();
  return x;
}
