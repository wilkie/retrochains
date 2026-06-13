int countLen(char *s) {
  int n = 0;
  while (*s != 0) {
    n++;
    s++;
  }
  return n;
}
int main(void) {
  return countLen("hello");
}
