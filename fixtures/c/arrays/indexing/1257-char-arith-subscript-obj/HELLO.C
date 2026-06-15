char arr[5];
int main(void) {
  int i;
  for (i = 0; i < 5; i++) arr[i] = i;
  return arr['B' - 'A'];
}
