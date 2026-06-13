int strlen_local(char *s) {
  int n = 0;
  while (*s++) n++;
  return n;
}
int main(void) {
  return strlen_local("Hello, World!");
}
