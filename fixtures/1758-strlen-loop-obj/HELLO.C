int my_strlen(char *s) {
  int n = 0;
  while (*s) {
    n++;
    s++;
  }
  return n;
}
int main(void) {
  return my_strlen("HELLO");
}
