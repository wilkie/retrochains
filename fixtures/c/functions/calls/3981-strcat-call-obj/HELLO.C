char *strcat(char *, char *);
int main(void) {
  char a[16];
  a[0] = 0;
  strcat(a, "ab");
  return a[0];
}
