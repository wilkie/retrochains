#define STR(x) #x
#define WORD STR(hello)
int main(void) {
  char buf[sizeof WORD];
  int i;
  int sum;
  for (i = 0; i < (int)(sizeof WORD); i++) {
    buf[i] = (char)('A' + i);
  }
  sum = 0;
  sum = sum + buf[0];
  sum = sum + (int)(sizeof buf);
  return sum - 'A';
}
